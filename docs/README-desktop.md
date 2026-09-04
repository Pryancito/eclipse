# Escritorio labwc de Eclipse OS

Eclipse OS incluye de serie una sesión de escritorio Wayland basada en
**labwc** (wlroots + renderizador software pixman, ver
[README-drm.md](README-drm.md)) con una apariencia propia: tema oscuro con
acento violeta, wallpaper nocturno con el logo de Eclipse y un panel inferior
con barra de tareas, reloj e indicadores.

Toda la configuración la genera `xtask` al construir el rootfs
(`xtask/src/linux/desktop.rs`), así que está presente desde el primer
arranque sin pasos manuales.

## Componentes

| Pieza | Archivo generado | Qué hace |
|---|---|---|
| **lunarbg** | `/bin/lunarbg` | Cliente de fondo **animado** de Eclipse OS (`tools/lunarbg`, Rust estático). Recrea el fondo del compositor smithay original de eclipse-old: media luna dorada central, anillo de texto «ECLIPSE-SYSTEM-KERNEL…» orbitando, tres arcos tech girando a velocidades distintas, anillos pulsantes y ticks técnicos, sobre base cósmica con estrellas y rejilla de 48 px. Dibuja proceduralmente a resolución nativa vía wlr-layer-shell + wl_shm (sin imágenes ni gdk-pixbuf), con render por *scanline spans* (~2 ms/frame a 1080p) y solo redibuja/daña la región del logo por frame. La animación apunta a 24 fps (`--fps`/`LUNARBG_FPS=1..60`) pero **cada commit se regula con el frame callback del compositor**: nunca renderiza por delante de lo que éste composita (en stacks lentos degrada sola, y con el fondo tapado cae a 1 Hz). Soporta HiDPI (`wl_output.scale` + `set_buffer_scale`), multi-monitor con aspecto físico por salida, `--output NAME` para pintar salidas concretas, pausa/reanuda con `SIGUSR1` y salida limpia con `SIGTERM`. `--static`/`LUNARBG_STATIC=1` desactiva la animación; debug: `--dump /tmp/out.raw:1920x1080` (+`--dump-ms N`) y `--bench` para cronometrar el renderizador. `lunarbg --help` lista todo. |
| Wallpaper (respaldo) | `/usr/share/backgrounds/eclipse/eclipse-night.png` | La misma escena, renderizada en build a PNG (encoder propio, sin dependencias). Solo se usa si lunarbg no está y toca recurrir a swaybg. |
| Tema de ventanas | `/usr/share/themes/Eclipse-Dark/openbox-3/themerc` | Tema openbox-3 oscuro que labwc aplica a bordes de ventana, menús y OSD. |
| Config labwc | `/root/.config/labwc/rc.xml` | Tema `Eclipse-Dark`, esquinas redondeadas, 4 escritorios y atajos de teclado. |
| Menú de escritorio | `/root/.config/labwc/menu.xml` | Clic derecho en el fondo: terminal, editor, monitor, recargar y salir. |
| Entorno de sesión | `/root/.config/labwc/environment` | Cursor Adwaita y `GTK_THEME=Adwaita:dark`. |
| Autoarranque | `/root/.config/labwc/autostart` | Lanza `swaybg` (wallpaper), `foot` (terminal) y `waybar` (panel, el último). Cada cliente está protegido con `command -v`: si falta, se anota en el log y la sesión sigue. |
| Panel | `/root/.config/waybar/{config,style.css}` | Barra inferior: lanzador + barra de tareas a la izquierda; CPU, memoria y reloj a la derecha. |
| GTK 3/4 | `/root/.config/gtk-{3.0,4.0}/settings.ini` | Modo oscuro por defecto para aplicaciones GTK. |
| Terminal | `/root/.config/foot/foot.ini` | Paleta violeta oscura a juego con el escritorio. |
| Lanzador (shell) | `/usr/local/bin/labwc` | Wrapper endurecido de labwc. Lo usan tanto los shells interactivos (`login` limpia env) como `eclipse-init`, para que ambos pasen por la misma selección de renderer y variables de entorno. |

## Paquetes de runtime

El núcleo y el rootfs no incluyen los binarios Wayland; se instalan desde
Alpine. Todos son opcionales — la sesión degrada con elegancia si falta
alguno:

```sh
apk add labwc seatd foot wayland-protocols font-dejavu adwaita-icon-theme
apk add sdl2 sdl3 sdl12-compat sdl2_image sdl2_ttf sdl2_mixer libdecor   # runtime SDL (ver "SDL")
```

Desde `cargo xtask image` estos paquetes ya se instalan solos (ver
`DEFAULT_PACKAGES` en `xtask/src/linux/xorg.rs`): `labwc` arrastra su cierre
de runtime (wlroots, wayland-libs, libinput, pixman, libxkbcommon), `seatd`
aporta `libseat.so` (el wrapper usa su backend `builtin`, sin demonio, válido
porque la sesión corre como root) y `foot` es el terminal.

- `labwc` — el compositor.
- `seatd` — gestor de asientos; wlroots abre /dev/dri y los nodos de entrada
  a traves de `libseat` (backend `builtin`, sin demonio).
- `swaybg` — solo como respaldo del fondo: `lunarbg` (incluido en el rootfs)
  pinta el fondo sin necesitar swaybg ni gdk-pixbuf.
- `waybar` — el panel inferior (sin él, no hay barra pero todo funciona).
- `foot` — terminal Wayland.
- `font-dejavu` — tipografía usada por tema, panel y menús.
- `adwaita-icon-theme` — tema de cursor e iconos (sin él no se ve el puntero
  con cursor software).

## Xwayland (aplicaciones X11)

labwc puede correr aplicaciones **X11 heredadas** dentro de la sesión Wayland
mediante **Xwayland**, un servidor X rootless. Eclipse lo arranca *con el
compositor*, no bajo demanda: el `rc.xml` que genera el build fija
`<core><xwaylandPersistence>yes</xwaylandPersistence></core>` (labwc >= 0.7.3).
El arranque perezoso quedó descartado porque su fallo no era ruidoso — labwc
escucha en el socket y **aparca** al cliente esperando un servidor que nunca
terminaba de arrancar, así que una app X11 se colgaba para siempre en vez de
fallar. Está habilitado de serie:

- El binario `Xwayland` se instala con el resto del stack (paquete `xwayland`
  en `DEFAULT_PACKAGES`, `xtask/src/linux/xorg.rs`) y viaja tanto al sistema
  instalado como al initramfs live/QEMU (`usr/bin` está en `LIVE_TREES`). El
  build lo verifica y avisa **en voz alta** si faltara.
- labwc de Alpine trae el soporte compilado; con `xwaylandPersistence` el
  servidor ya está en pie cuando arranca la sesión, y labwc exporta el número
  de display real a sus hijos (por eso una terminal de la sesión hereda
  `DISPLAY` sin que nadie lo fije a mano).
- `DISPLAY=:0` se fija en el entorno de sesión (`CHILD_ENV` en
  `tools/eclipse-init/src/main.rs`, y espejado en el `environment` de labwc),
  de modo que una app X11 lanzada a mano desde un terminal `foot` encuentra el
  servidor. Con un solo compositor y sin otro servidor X, ese display siempre
  es `:0`.

Los clientes Wayland nativos (foot, lunarbg, lunarbar) ignoran `DISPLAY`, y los
toolkits GTK/Qt siguen prefiriendo Wayland porque `WAYLAND_DISPLAY` está
presente — así que fijar `DISPLAY` no cambia su comportamiento; solo da a las
apps X11-only un servidor al que conectarse.

Comprobar que funciona desde una terminal de la sesión:

```sh
echo $DISPLAY          # -> :0
glxgears               # engranajes GL (vía Xwayland + el GL por hardware nouveau)
xterm                  # una terminal X11 clásica dentro del escritorio Wayland
```

Si una app X11 falla con «can't open display» aun con `DISPLAY=:0`, revisa en
la salida del build la línea `Xorg stack: Xwayland present …`: si dice que
falta, `apk` no resolvió el paquete `xwayland` (lo más común, sin red al
construir la imagen) y hay que reconstruir con red o `apk add xwayland` una vez.

## SDL (SDL 1.2 / SDL2 / SDL3)

La sesión trae el runtime de **SDL** (`sdl2`, `sdl3`, `sdl12-compat`,
`sdl2_image`, `sdl2_ttf`, `sdl2_mixer`, `libdecor`; ver `DEFAULT_PACKAGES` en
`xtask/src/linux/xorg.rs`) y una **política de entorno** que elige el backend
de vídeo y el *render driver* de SDL a juego con el renderer del compositor.
Sin ella, SDL2 tomaba X11 siempre (elige X11 en cuanto `DISPLAY` está fijado, y
aquí lo está: `:0`), es decir, todo pasaba por Xwayland, y el renderer lo
decidía Mesa a ciegas.

La política se asierta en los mismos cuatro sitios que la del renderer de
wlroots, para que una app SDL lanzada desde una shell y otra lanzada por init
rendericen por la misma pila: el wrapper `/usr/local/bin/labwc`,
`/etc/profile`, `~/.config/labwc/environment` (solo la mitad estática) y
`eclipse-init` (`push_sdl_render_env`). Un test de `xtask`
(`sdl_policy_is_consistent_across_wrapper_profile_and_environment`) comprueba
que las copias generadas no se desalinean.

| Sesión | `WLR_RENDERER` | `SDL_RENDER_DRIVER` | `SDL_FRAMEBUFFER_ACCELERATION` | Qué hace SDL |
|---|---|---|---|---|
| pixman (por defecto) | `pixman` | `software` | `0` | Rasteriza en CPU. **SDL3** presenta por `wl_shm`, sin tocar GL (la misma ruta que foot/lunarbg). **SDL2** no tiene framebuffer shm en Wayland y presenta por EGL, que `LIBGL_ALWAYS_SOFTWARE=1` deja en llvmpipe. |
| gles2 (`nvidia.wlr_gles2`, o `renderer=gl-sw`/`GL=1` en QEMU) | `gles2` | `opengles2` | `opengles2` | Renderer GLES2 sobre la misma pila GL que el compositor: zink+NVK en hardware, llvmpipe en software. |
| vulkan (`nvidia.wlr_vulkan`) | `vulkan` | `opengles2` | `opengles2` | Igual que gles2: SDL2 no tiene renderer Vulkan, y GLES2 acaba en zink+NVK. |

Independientes del renderer, en todas las sesiones:

- `SDL_VIDEODRIVER=wayland,x11` (SDL2) y `SDL_VIDEO_DRIVER=wayland,x11`
  (SDL3): Wayland nativo primero, X11 como respaldo. La lista sirve también a
  la sesión `desktop=xorg` (sin `WAYLAND_DISPLAY`, SDL cae a X11 solo). La
  sintaxis con comas exige SDL >= 2.24 (Alpine trae 2.30+).
- `SDL_AUDIODRIVER=alsa` / `SDL_AUDIO_DRIVER=alsa`: ALSA es la única API de
  audio de usuario que expone este kernel ([README-audio.md](README-audio.md));
  el pin evita que SDL sondee antes los sockets de pipewire/pulse.
- Decoraciones: labwc ofrece decoración de servidor por `xdg-decoration`, que
  SDL prefiere; `libdecor` queda como respaldo del lado cliente.

Todas las variables se fijan con `:=` en el wrapper, así que un override del
que lanza gana (p. ej. `SDL_RENDER_DRIVER=opengles2 mi-juego` para forzar la
ruta GL de un cliente concreto en la sesión pixman).

**Comprobar que funciona** desde un terminal de la sesión (o desde el menú de
escritorio, entradas «Prueba SDL2 (renderer)» y «Prueba SDL3 (wl_shm)»):

```sh
eclipse-sdl-probe                    # SDL2 + SDL_Renderer: espera 'video driver in use: wayland'
eclipse-sdl-probe --sdl3 --surface   #   y "renderer 'software'" en pixman / 'opengles2' en gles2
```

`eclipse-sdl-probe` (`tools/eclipse-sdl-probe`) abre la libSDL instalada con
`dlopen`, imprime versión, backends compilados, backend y renderer en uso, y
mide los fps de una animación sencilla; sale con `SDLPROBE: FAIL ...` y código
1 si algo falla. Un `video driver in use: x11` con `WAYLAND_DISPLAY` presente
significa que algo pisó `SDL_VIDEODRIVER`.

## Juegos (supertux2, gzdoom/freedoom)

Los dos se instalan desde los repos de Alpine y se lanzan desde un terminal de
la sesión (o desde el menú si se añade una entrada):

```sh
apk add supertux gzdoom freedoom     # binarios: supertux2, gzdoom, freedoom1, freedoom2
supertux2
freedoom2                            # = gzdoom -iwad freedoom2.wad
```

Los dos fallaban en hardware real por causas distintas, y ambas están tapadas
a la vez por el kernel y por la política de entorno de la sesión:

- **supertux2** abortaba con `Assertion 'r == 0 || r == 95' failed at
  ../src/pulsecore/mutex-posix.c:57, function pa_mutex_new()`. SuperTux (y
  gzdoom) usan OpenAL; openal-soft prueba primero pipewire y pulse, y cargar
  libpulse ejecuta `pa_mutex_new()`, que llama a
  `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)`. musl sondea el kernel
  con `FUTEX_LOCK_PI` y devuelve **tal cual** el errno del kernel (no hay
  traducción: `if (r) return r;` en `pthread_mutexattr_setprotocol.c`), y
  PulseAudio solo acepta 0 o `ENOTSUP`. El kernel implementa ahora
  `FUTEX_LOCK_PI`/`FUTEX_LOCK_PI2`/`FUTEX_TRYLOCK_PI`/`FUTEX_UNLOCK_PI`
  (`linux-syscall/src/misc.rs`, protocolo de palabra de bloqueo de Linux:
  TID del dueño, `FUTEX_WAITERS`, `FUTEX_OWNER_DIED`), así que la sonda
  devuelve 0 y los mutex PI funcionan de verdad. Además la sesión exporta
  `ALSOFT_DRIVERS=alsa`, de modo que openal-soft ni siquiera carga libpulse:
  ALSA es la única API de audio de usuario que hay ([README-audio.md](README-audio.md)).
- **gzdoom** se quedaba colgado justo tras imprimir `GZDoom 4.14.2 - - SDL
  version / Compiled on ...`: lo siguiente que hace `main()` es `SDL_Init(0)`,
  y SDL2 (con cualquier máscara de subsistemas) ejecuta antes que nada
  `SDL_DBus_Init()`, que pide el bus de sesión a libdbus. Sin
  `DBUS_SESSION_BUS_ADDRESS`, libdbus usa `autolaunch:`: hace fork de
  `dbus-launch`, que abre `$DISPLAY` (Xwayland) y lanza un `dbus-daemon` más
  un proceso «niñera» detrás de tuberías, esperando EOF. La sesión exporta
  ahora `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus` en los mismos
  cuatro sitios que la política de SDL (wrapper, `/etc/profile`,
  `~/.config/labwc/environment`, `eclipse-init`): sin demonio el `connect`
  falla al instante (`ECONNREFUSED`), SDL desactiva D-Bus y el juego sigue.
  Si algún día hace falta un bus, basta con arrancarlo en esa ruta
  (`dbus-daemon --session --address=unix:path=$XDG_RUNTIME_DIR/bus --fork`) y
  todos los clientes nuevos lo usan sin tocar nada más.

gzdoom necesita OpenGL 3.3+ (o Vulkan): en la sesión pixman va por llvmpipe
(`LIBGL_ALWAYS_SOFTWARE=1`), que sirve pero es lento; en las sesiones
`nvidia.wlr_gles2`/`nvidia.wlr_vulkan` usa zink+NVK. Con varios IWAD y sin
`-iwad`, gzdoom abre su selector GTK; los lanzadores `freedoom1`/`freedoom2`
ya pasan el IWAD.

## Atajos de teclado

| Atajo | Acción |
|---|---|
| `Super+Enter` / `Alt+Enter` | Abrir terminal (`foot`) |
| `Super+Espacio` | Menú de escritorio |
| `Super+K` | Ciclar layout de teclado (`es` / `us`) |
| `Alt+Tab` | Cambiar de ventana |
| `Alt+F4` | Cerrar ventana |
| `Super+↑` | Maximizar / restaurar |
| `Super+←` / `Super+→` | Anclar a media pantalla |
| `Super+1..4` | Ir al escritorio N |
| `Super+Shift+1..4` | Mover ventana al escritorio N |

El layout empieza en `es` (consola, labwc y Xwayland). Se cambia en caliente
con `eclipse-kbd toggle` (o `es`/`us`), el atajo `Super+K`, la píldora **ES/US**
del panel, o `echo us > /proc/kbd` más `eclipse-kbd us` para el escritorio.
Queda en `/etc/eclipse/keyboard`; en el arranque manda `kbd=us` en la cmdline.

## El panel y la estabilidad del sistema

waybar es una aplicación GTK, y en este hardware la ruta GL/GBM puede colgar
el sistema completo (ver la nota del wrapper `/usr/local/bin/labwc`). El
autoarranque lo protege por partida triple:

1. Se lanza con `GDK_GL=disable`, de modo que GTK renderiza por
   cairo/shm — el mismo camino que swaybg y foot, que funcionan bien aquí.
2. La configuración solo usa módulos que dependen del socket Wayland y de
   `/proc` (taskbar, reloj, CPU, memoria). Los módulos `tray` (dbus),
   `network` (netlink) y `pulseaudio` ejercitan rutas del kernel todavía
   parciales en Eclipse OS: añádelos de uno en uno solo tras probarlos.
3. Un candado anti-bucle: antes de lanzar waybar se crea
   `~/.config/labwc/panel.lock`, que se borra cuando el panel sobrevive
   15 s. Si la sesión muere con el candado puesto (cuelgue, apagón), el
   siguiente arranque **salta waybar automáticamente** y lo anota en el
   log. Para reintentar: `rm ~/.config/labwc/panel.lock`.

## Diagnóstico

El autoarranque registra todo en `~/.config/labwc/autostart.log`, de modo que
un escritorio negro se diagnostica **sin reiniciar**:

```sh
cat ~/.config/labwc/autostart.log
```

Cada cliente que falte aparece como `MISSING <cliente>` con el `apk add`
necesario. La línea `wallpaper:` registra el `ls -l` del PNG.

**Fondo liso en vez de la escena nocturna.** Si swaybg registra
`Failed to load image` / `Couldn't recognize the image file format` con el
PNG presente en disco, es gdk-pixbuf sin su `loaders.cache`: apk lo genera
con un *trigger* que puede no haberse ejecutado bajo Eclipse OS, y sin él
gdk-pixbuf no reconoce **ningún** formato de imagen (swaybg no carga fondos
y las apps GTK pierden sus iconos). El autoarranque lo detecta y ejecuta
`gdk-pixbuf-query-loaders --update-cache` automáticamente; además, si
swaybg muere al cargar la imagen, un vigilante relanza el fondo con color
sólido a los 2 s para que el escritorio nunca se quede sin fondo.

Si el sistema se cuelga al arrancar la sesión y necesitas entrar sin
escritorio: cambia a otra consola virtual (`Ctrl+Alt+F2`) antes de lanzar
labwc y comenta la línea de waybar en `~/.config/labwc/autostart` (o borra
`panel.lock` solo cuando quieras reintentar el panel).

## Personalización

- **Wallpaper**: sustituye `/usr/share/backgrounds/eclipse/eclipse-night.png`
  o edita la ruta en `~/.config/labwc/autostart`. Para regenerar el original
  fuera de un build completo:
  `cargo test -p xtask dump_wallpaper -- --ignored` (lo escribe en el
  directorio temporal, o en `$ECLIPSE_WALLPAPER_OUT`).
- **Colores del tema**: edita
  `/usr/share/themes/Eclipse-Dark/openbox-3/themerc` y ejecuta la acción
  «Recargar labwc» del menú (o `labwc --reconfigure`).
- **Panel**: `~/.config/waybar/config` y `style.css`; reinicia waybar
  (`pkill waybar; waybar &`).

Ten en cuenta que los archivos bajo `/root/.config` y `/usr/share` los
escribe `xtask` al construir el rootfs: los cambios persistentes deben
hacerse en `xtask/src/linux/desktop.rs`.

## Elegir la sesión: labwc o Xorg

Eclipse trae dos sesiones de escritorio y `eclipse-init` (PID 1) arranca solo
una, según este orden (gana el primero que aparezca):

1. **Argumento de arranque** `desktop=<labwc|xorg>` en la cmdline del kernel
   (`/proc/cmdline`). Es lo que usa `make qemu`, que fija `desktop=xorg` para
   arrancar la sesión Xorg (framebuffer/`fbdev` sobre `/dev/fb0`) en QEMU.
   Sobrescríbelo con `make qemu DESKTOP=labwc`.
2. **Fichero** `/etc/eclipse/desktop` (primer token): override persistente por
   instalación que puedes editar. Se instala con el valor `labwc`.
3. Por defecto: **labwc**.

Como el hardware real arranca con la cmdline instalada (sin `desktop=`), cae al
fichero `/etc/eclipse/desktop` y usa **labwc**; `make qemu` usa **Xorg**. Los
servicios de cada sesión llevan una etiqueta `desktop =` en su
`*.service` (`/etc/eclipse/services/`): `seatd`/`labwc` son `desktop = labwc`,
`xorg` es `desktop = xorg`, y los servicios sin etiqueta (p. ej. `udhcpc`)
arrancan siempre.

`make vbox` arranca la **misma** sesión live que `make qemu` (`desktop=labwc`,
initramfs con Mesa/labwc). El ISO (`make iso` / `./scripts/vbox-eclipse.sh --iso`)
es el instalador: cmdline `desktop=none` y SFS sin escritorio, así que
`eclipse-init` deja la consola y no lanza el compositor. Tras `install-eclipse`,
`./scripts/vbox-eclipse.sh --disk-only` arranca labwc desde el VDI instalado.
