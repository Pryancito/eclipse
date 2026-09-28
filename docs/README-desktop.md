# Escritorio labwc de Eclipse OS

Eclipse OS incluye de serie una sesión de escritorio Wayland basada en
**labwc** (wlroots + renderizador software pixman, ver
[README-drm.md](README-drm.md)), con **tres apariencias** intercambiables
sobre exactamente las mismas piezas nativas: la original de Eclipse (la que
trae la imagen), la de KDE Plasma y la de Windows 11. El fondo siempre es
**lunarbg**, el panel siempre es **lunarbar** y el lanzador siempre es
**lunarrun**; lo que cambia es la paleta y la disposición.

## Las tres apariencias

Se eligen con `eclipse-look` y se guardan en `/etc/eclipse/look` (manda
`look=` de la cmdline si está). `eclipse-init` la aplica en el arranque,
igual que hace con el idioma y la zona horaria.

```sh
eclipse-look            # imprime la actual
eclipse-look eclipse    # la violeta original (por defecto)
eclipse-look kde        # KDE Breeze Dark
eclipse-look win11      # Windows 11
```

| | `eclipse` (por defecto) | `kde` | `win11` |
|---|---|---|---|
| Tema de ventanas | `Eclipse-Dark` (violeta) | `Breeze-Dark` (#31363b, acento #3daee9) | `Win11-Dark` (#202020, acento #0078d4) |
| Panel | dos barras de 34 px (info arriba, tareas abajo) | una barra inferior de 44 px, estilo Plasma | una barra inferior de 48 px, **botones centrados**, translúcida |
| Reloj | una línea abajo, fecha arriba | dos líneas, hora sobre fecha | dos líneas, hora sobre fecha |
| Terminal | paleta violeta | paleta Breeze (Konsole) | paleta Campbell (Windows Terminal) |
| Lanzador | en el tercio superior | en el tercio superior | menú Inicio sobre la barra |

Un cambio de apariencia reescribe el `<name>` del tema en `rc.xml`, copia la
paleta de foot correspondiente y reinicia el panel (init lo relanza al
instante); `labwc --reconfigure` recarga el tema sin cerrar la sesión.

**Sombras**: los tres `themerc` piden sombra bajo las ventanas
(`window.active.shadow.size`/`.color`), que labwc dibuja de verdad desde
0.8. Una versión anterior ignora las claves que no conoce, así que no
rompe nada donde no haya soporte. Esto sí es una sombra del compositor; lo
que no hay es desenfoque.

**Translucidez**: en `win11` la barra se dibuja sobre un búfer ARGB al 85 % y
el menú del lanzador al 92 %, así que el fondo se transparenta. Es
translucidez **plana**: el acrílico y el Mica de Windows desenfocan lo que
hay detrás de la superficie y labwc no sabe desenfocar, así que no hay
efecto esmerilado. No se distribuye ninguna fuente, icono ni fondo de
Microsoft: todo está dibujado con colores propios.

## KDE: lo que hay, lo que falta y por qué

Desde que hay bus de sesión de verdad (ver [README-dbus.md](README-dbus.md)),
la imagen trae **Qt 6, KDE Frameworks 6 y las aplicaciones de KDE**: Alpine
3.24 empaqueta Plasma 6.6 y KF6 6.26. La lista está en `KDE_PACKAGES`
(`xtask/src/linux/xorg.rs`) e incluye `kded` (el demonio `kded6`),
`plasma-integration`, `qt6-qtwayland`, `breeze`, `breeze-icons`, `kio-extras`,
`kwallet`, `kde-cli-tools`, `xdg-desktop-portal-kde` y las aplicaciones
(dolphin, konsole, kate, ark, okular, gwenview, kcalc, spectacle). Son del
orden de **1,2 GiB instalados**; `ECLIPSE_KDE=0` los deja fuera.

Lo que hace que además **funcionen**, y no solo se instalen:

- **Activación de servicios en el bus.** KDE abre kiod, kwalletd o un módulo
  de kded pidiéndole al bus que lo arranque. Eso lo hace el `dbus-daemon` de
  Alpine, que ya está en la imagen; `eclipse-dbusd`, el demonio propio de
  respaldo, contesta `ServiceUnknown` a `StartServiceByName` a propósito, así
  que con él las apps conectan y luego se quedan esperando. Para KDE hace
  falta el `dbus-daemon` de verdad.
- **`kded6` como servicio de init**, no por autoarranque de labwc
  (`/etc/eclipse/services/kded.service` → `/usr/local/bin/eclipse-kded`). El
  servicio espera a las dos cosas que kded6 necesita antes de arrancarlo:
  `wait_socket` al socket de Wayland (construye una `QGuiApplication`) y
  `wait_path` al socket del bus. Sin bus, kded6 sale al instante y
  `type = respawn` lo repetiría en bucle toda la sesión.
- **`QT_QPA_PLATFORMTHEME=kde` solo si el plugin está**. Lo exporta el wrapper
  de labwc después de comprobar que existe
  `…/qt6/plugins/platformthemes/KDEPlasmaPlatformTheme*.so`, porque `apk` aquí
  es *best-effort* y un tema que se nombra sin tenerlo instalado hace que cada
  aplicación Qt avise al arrancar y luego use el de siempre.
- **`XDG_CURRENT_DESKTOP=KDE:labwc:wlroots`** cuando la imagen se construyó con
  KDE (si no, `labwc:wlroots` a secas). El primer nombre es el backend de
  portal que pide la sesión, y sigue siendo una sesión wlroots para todo lo
  demás.
- **Colores**: `~/.config/kdeglobals` fija estilo Breeze, iconos breeze-dark y
  doble clic; las paletas `[Colors:*]` las copia del `BreezeDark.colors` del
  propio paquete `eclipse-kde-colors` en el primer arranque, para no llevar
  una copia a mano que se desvíe de la de verdad.

Lo que **sigue sin poder funcionar**, y no se arregla instalando nada:

- **No hay bus de sistema**, así que polkit (acciones de root en
  systemsettings), accountsservice (nombre y avatar del usuario) y logind
  (asiento, suspensión, cierre de sesión) no tienen con quién hablar. Aquí el
  asiento lo da seatd. Esas piezas se degradan a «no disponible», no rompen la
  aplicación.
- **El panel de Plasma no puede listar ventanas sobre labwc.** El gestor de
  tareas de Plasma habla `org_kde_plasma_window_management` y nada más
  (`plasma-workspace/libtaskmanager/waylandtasksmodel.cpp`), y labwc solo añade
  `wlr-layer-shell` y `wlr-output-power-management` a lo que trae wlroots
  (`labwc/protocols/meson.build`): el protocolo de KDE no está en ninguno de
  los dos. Un `plasmashell` sobre labwc arranca —lanzador, reloj y bandeja van,
  porque son layer-shell y D-Bus— con la barra de tareas **vacía para
  siempre**.
- Por eso el shell de Plasma (`plasma-workspace`, `plasma-desktop`,
  `systemsettings`) es un conjunto aparte y **apagado por defecto**:
  `ECLIPSE_PLASMA=1` lo añade, y arrastra kwin, accountsservice, fprintd y un
  gestor de sesión de pipewire como dependencias duras. Tiene sentido para
  intentar la sesión Plasma completa bajo `kwin_wayland` (que sí implementa su
  propio protocolo y sabe usar libseat), no para pegarle el panel de Plasma a
  labwc.

El panel y el lanzador de la sesión siguen siendo `lunarbar` y `lunarrun`,
que sí listan ventanas por `wlr-foreign-toplevel-management`.

Toda la configuración la genera `xtask` al construir el rootfs
(`xtask/src/linux/desktop.rs`), así que está presente desde el primer
arranque sin pasos manuales.

## Componentes

| Pieza | Archivo generado | Qué hace |
|---|---|---|
| **lunarbg** | `/bin/lunarbg` | Cliente de fondo **animado** de Eclipse OS (`tools/lunarbg`, Rust estático). Recrea el fondo del compositor smithay original de eclipse-old: media luna dorada central, anillo de texto «ECLIPSE-SYSTEM-KERNEL…» orbitando, tres arcos tech girando a velocidades distintas, anillos pulsantes y ticks técnicos, sobre base cósmica con estrellas y rejilla de 48 px. Dibuja proceduralmente a resolución nativa vía wlr-layer-shell + wl_shm (sin imágenes ni gdk-pixbuf), con render por *scanline spans* (~2 ms/frame a 1080p) y solo redibuja/daña la región del logo por frame. La animación apunta a 24 fps (`--fps`/`LUNARBG_FPS=1..60`) pero **cada commit se regula con el frame callback del compositor**: nunca renderiza por delante de lo que éste composita (en stacks lentos degrada sola, y con el fondo tapado cae a 1 Hz). Soporta HiDPI (`wl_output.scale` + `set_buffer_scale`), multi-monitor con aspecto físico por salida, `--output NAME` para pintar salidas concretas, pausa/reanuda con `SIGUSR1` y salida limpia con `SIGTERM`. `--static`/`LUNARBG_STATIC=1` desactiva la animación; debug: `--dump /tmp/out.raw:1920x1080` (+`--dump-ms N`) y `--bench` para cronometrar el renderizador. `lunarbg --help` lista todo. |
| Wallpaper estático | `/usr/share/backgrounds/eclipse/eclipse-night.png` | La misma escena, renderizada en build a PNG (encoder propio, sin dependencias). Hoy ningún componente de la sesión la usa (swaybg no forma parte de ella); queda como imagen de respaldo para quien quiera un fondo estático. |
| Temas de ventanas | `/usr/share/themes/{Win11-Dark,Breeze-Dark,Eclipse-Dark}/openbox-3/themerc` | Los tres temas openbox-3 que labwc aplica a bordes de ventana, menús y OSD. `eclipse-look` elige cuál nombra `rc.xml`. |
| Config labwc | `/root/.config/labwc/rc.xml` | Tema de la apariencia activa (`Eclipse-Dark` de fábrica), esquinas redondeadas, 4 escritorios y los atajos de KDE/Windows. |
| Menú de escritorio | `/root/.config/labwc/menu.xml` | Clic derecho en el fondo: terminal, editor, monitor, recargar y salir. |
| Entorno de sesión | `/root/.config/labwc/environment` | Cursor Adwaita y `GTK_THEME=Adwaita:dark`. |
| **lunarrun** | `/bin/lunarrun` | Lanzador tipo KRunner (`tools/lunarbar`, comparte biblioteca con el panel). Overlay centrado sobre wlr-layer-shell: se escribe para filtrar las aplicaciones instaladas (sin distinguir acentos), ↑/↓ o Tab para elegir, Intro para lanzar, Esc o clic fuera para cerrar. Una palabra que resuelva en `$PATH` sale como «ejecutar orden», así que `Alt+Espacio top` funciona como en KRunner. `lunarrun --toggle-desktop` es el Super+D de KDE: minimiza todas las ventanas por wlr-foreign-toplevel-management, o las restaura si ya lo estaban. `--dump RUTA:AnchoxAlto` lo dibuja a un fichero ARGB8888 sin compositor. Lo lanzan `eclipse-run` y `eclipse-showdesktop`. |
| **lunarbar** | `/bin/lunarbar` | Panel propio de Eclipse OS (`tools/lunarbar`, Rust estático, wlr-layer-shell + wl_shm, sin GTK ni GL): una barra inferior por salida en `win11`/`kde` (dos en `eclipse`) con lanzador, barra de tareas, reloj, volumen, teclado y apagado, más popups (menú de aplicaciones) y tooltips. Traducido (`i18n.rs`). |
| **kded6** | `/usr/local/bin/eclipse-kded` + `/etc/eclipse/services/kded.service` | Demonio de servicios de KDE (el `kded6` de KF6), donde se cargan los módulos de fondo de KDE. El servicio espera al socket de Wayland y al del bus antes de arrancarlo (esperas nativas de init, sin bucles de shell) y, si `kded6` no está instalado, el wrapper lo dice en `/tmp/kded.log` y en la consola en vez de dejar que init lo reintente en bucle. |
| Ajustes de KDE | `/root/.config/kdeglobals` | Estilo Breeze, iconos breeze-dark y doble clic para todas las aplicaciones KF6 y, vía plasma-integration, para cualquier app Qt. Las paletas `[Colors:*]` las añade `eclipse-kde-colors` (servicio `oneshot`) copiando el `BreezeDark.colors` que trae el paquete `breeze`, y solo si no están ya. |
| Autoarranque | *(ausente a propósito)* | labwc lanza `sh ~/.config/labwc/autostart` con doble `fork`, y esa `ash` cae con SIGSEGV en este kernel (musl mallocng). En su lugar `eclipse-init` arranca el fondo y el panel como **servicios** (`/etc/eclipse/services/{lunarbg,lunarbar}.service`, con `after = labwc` y `wait_socket` sobre `wayland-0`) a través de los wrappers `/usr/local/bin/eclipse-lunarbg` y `eclipse-lunarbar`, que a su vez esperan al socket `wayland-*`. `~/.config/labwc/autostart.README` lo explica en el sistema instalado. |
| GTK 3/4 | `/root/.config/gtk-{3.0,4.0}/settings.ini` | Modo oscuro por defecto para aplicaciones GTK. |
| Terminal | `/root/.config/foot/foot.ini` (+ `foot.win11.ini`, `foot.kde.ini`, `foot.eclipse.ini`) | Paleta a juego con la apariencia activa; `eclipse-look` copia la que toque sobre `foot.ini`. |
| Lanzador (shell) | `/usr/local/bin/labwc` | Wrapper endurecido de labwc. Lo usan tanto los shells interactivos (`login` limpia env) como `eclipse-init`, para que ambos pasen por la misma selección de renderer y variables de entorno. |

## Paquetes de runtime

El núcleo y el rootfs no incluyen los binarios Wayland; se instalan desde
Alpine. Todos son opcionales — la sesión degrada con elegancia si falta
alguno:

```sh
apk add labwc seatd foot wayland-protocols font-dejavu adwaita-icon-theme
apk add sdl2 sdl3 sdl12-compat sdl2_image sdl2_ttf sdl2_mixer sdl2_net libpng fluidsynth libdecor   # runtime SDL (ver "SDL")
```

Desde `cargo xtask image` estos paquetes ya se instalan solos (ver
`DEFAULT_PACKAGES` en `xtask/src/linux/xorg.rs`): `labwc` arrastra su cierre
de runtime (wlroots, wayland-libs, libinput, pixman, libxkbcommon), `seatd`
aporta `libseat.so` y el demonio `seatd`, y `foot` es el terminal.

- `labwc` — el compositor.
- `seatd` — gestor de asientos; corre como **demonio** (servicio de init,
  socket `/run/seatd.sock`, sobre el que `labwc.service` espera). El wrapper
  no fija `LIBSEAT_BACKEND=builtin`: la `libseat` de Alpine no compila ese
  backend.
- `lunarbg` y `lunarbar` — fondo y panel, incluidos en el rootfs (no son
  paquetes de Alpine); no hace falta `swaybg` ni `waybar`.
- `foot` — terminal Wayland.
- `font-dejavu` — tipografía usada por tema, panel y menús.
- `adwaita-icon-theme` — tema de cursor e iconos (sin él no se ve el puntero
  con cursor software).
- KDE (`KDE_PACKAGES`, ~1,2 GiB): `kded`, `plasma-integration`,
  `qt6-qtwayland`, `breeze`, `breeze-icons`, `kio-extras`, `kwallet`,
  `kde-cli-tools`, `xdg-desktop-portal-kde` y las aplicaciones. `ECLIPSE_KDE=0`
  las deja fuera; `ECLIPSE_PLASMA=1` añade además el shell de Plasma. Ver
  «KDE: lo que hay, lo que falta y por qué».

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
`sdl2_image`, `sdl2_ttf`, `sdl2_mixer`, `sdl2_net`, `libpng`, `fluidsynth`,
`libdecor`; ver `DEFAULT_PACKAGES` en
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
- `SDL_AUDIODRIVER=alsa` / `SDL_AUDIO_DRIVER=alsa`: SDL usa ALSA, y
  `/etc/asound.conf` lo enruta por PulseAudio para que varios clientes
  compartan el PCM ([README-audio.md](README-audio.md)). `PULSE_SERVER`
  apunta a `unix:/run/pulse/native` para los que hablan libpulse.
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

## Juegos (freedoom, supertux2)

**Freedoom viene instalado.** Los dos IWAD (`freedoom1.wad`, `freedoom2.wad`) y
el motor `gzdoom` están en el conjunto de paquetes por defecto
(`xtask/src/linux/xorg.rs`), porque un Eclipse recién instalado no tiene mirror
a mano justo cuando a uno le apetece ver si el escritorio mueve un juego.

Se lanza desde el menú del escritorio («Freedoom (Fase 1)» / «(Fase 2)»), desde
el menú de aplicaciones del panel, o a mano:

```sh
eclipse-freedoom          # Fase 2 (por defecto)
eclipse-freedoom 1        # Fase 1
eclipse-freedoom /ruta/doom2.wad   # cualquier otro IWAD
```

`eclipse-freedoom` (`xtask/src/linux/desktop.rs`) resuelve tres cosas:

- **Pasa siempre `-iwad`.** Con varios IWAD y sin esa opción, gzdoom abre un
  diálogo GTK para preguntar a cuál jugar; es lo primero que vería el usuario.
- **Elige renderizador.** gzdoom necesita OpenGL 3.3+ o Vulkan. En la sesión
  pixman no hay driver GL, así que el wrapper fuerza `LIBGL_ALWAYS_SOFTWARE=1`
  (llvmpipe: sirve, pero es lento); en las sesiones `nvidia.wlr_gles2` /
  `nvidia.wlr_vulkan` (`WLR_RENDERER` = `gles2`/`vulkan`) no toca el entorno,
  para que use zink+NVK.
- **Deja rastro.** Todo va a `/tmp/freedoom.log`, porque un juego lanzado desde
  el panel escribe en un terminal que no mira nadie.

Si hay instalado un motor por software (`chocolate-doom`, `crispy-doom`,
`prboom-plus`), el wrapper lo prefiere a gzdoom: en este stack, cuyo mejor GL es
llvmpipe, es el que va a velocidad completa.

```sh
apk add chocolate-doom    # opcional; eclipse-freedoom lo usará solo
apk add supertux          # binario: supertux2
```

Los dos juegos fallaban en hardware real por causas distintas, y ambas están
resueltas:

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
  devuelve 0 y los mutex PI funcionan de verdad. La sesión exporta
  `ALSOFT_DRIVERS=pulse,alsa` y `PULSE_SERVER=unix:/run/pulse/native`:
  openal-soft usa libpulse contra el demonio de sistema, y si Pulse no
  está cae a ALSA (que `/etc/asound.conf` también enruta por Pulse).
  Ver [README-audio.md](README-audio.md).
- **gzdoom** se quedaba colgado justo tras imprimir `GZDoom 4.14.2 - - SDL
  version / Compiled on ...`, sordo incluso a `^C`. **No era el bus de sesión**:
  con `DBUS_SESSION_BUS_ADDRESS=unix:path=/sin-bus` se colgaba igual. Eran dos
  fallos del kernel en el camino de `__synccall` de musl, que es lo que corre
  `seteuid()`: el hilo que llama bloquea todas las señales, manda
  `SIGSYNCCALL` a cada uno de los demás hilos y espera a que todos fichen.
  (1) El volcado de mapeos daba los permisos de la **primera** página del
  mapeo a todo él, y musl pone la página de guarda de cada pila de hilo
  **abajo**, así que toda pila de hilo parecía no escribible y el kernel no
  podía dejar ahí el marco de la señal (`nowhere to put the SIGRT34 frame`).
  (2) Un `FUTEX_WAIT` sin plazo no era interrumpible por nada, así que un hilo
  dentro de `pthread_cond_wait` no fichaba nunca. Los dos están arreglados
  ([PR #1601](https://github.com/Pryancito/eclipse/pull/1601)) y gzdoom
  arranca. El bozal de D-Bus del wrapper se queda por higiene, pero no
  arreglaba nada; ver [README-dbus.md](README-dbus.md).

## Atajos de teclado

| Atajo | Acción |
|---|---|
| `Alt+Espacio` / `Alt+F2` / `Ctrl+Alt+Supr` | Lanzador de búsqueda (`lunarrun`) |
| `Super+Enter` / `Alt+Enter` / `Ctrl+Alt+T` | Abrir terminal (`foot`) |
| `Super+E` | Gestor de archivos (`eclipse-files`) |
| `Super+D` | Mostrar escritorio (minimiza o restaura todo) |
| `Ctrl+F1..F4` | Ir al escritorio N (las teclas de KDE) |
| `Alt+Shift+Tab` | Ventana anterior |
| `Super+↓` | Minimizar |
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

El idioma de la UI es independiente del teclado. Por defecto `es`
(`LANG=es_ES.UTF-8`, `LANGUAGE=es:en`). `lang=en` en la cmdline o
`eclipse-locale en` (persistido en `/etc/eclipse/locale`) selecciona inglés
en labwc, lunarbar, Firefox/GTK (gettext) y el menú. No se exporta `LC_ALL`.

La zona horaria sigue el **país**, no el teclado: por defecto España
(`TZ=Europe/Madrid`, `/etc/eclipse/timezone`). `country=US` o
`eclipse-tz US` pasa a `America/New_York`; `tz=Europe/Paris` fija un Olson
arbitrario. `ntpd` (OpenNTPD, o busybox si está el applet) arranca con el
sistema tras DHCP y pone el reloj.

## El panel y la estabilidad del sistema

El panel es `lunarbar`, un cliente Wayland nativo (wl_shm, sin GTK, sin GL),
así que no pasa por la ruta GL/GBM que en este hardware puede colgar el
sistema (ver la nota del wrapper `/usr/local/bin/labwc`). Lo supervisa
`eclipse-init`: si muere se relanza con *backoff* exponencial y la línea
`respawn:` de la consola dice cómo terminó (`exit N` / `signal N`).

## Diagnóstico

- `labwc` escribe en `/tmp/labwc.log` (destino de `labwc.service` y del
  wrapper).
- Cada servicio que muere aparece en la consola (`[eclipse-init] respawn:
  ... exited after ...`), y un cliente ausente lo anuncia su wrapper en el
  log y en `/dev/console` (`not installed`) antes de esperar 60 s.
- `lunarbg --dump /tmp/out.raw:1920x1080` y `lunarbg --bench` permiten
  comprobar el renderizador sin compositor.

Si el sistema se cuelga al arrancar la sesión y necesitas entrar sin
escritorio: cambia a otra consola virtual (`Ctrl+Alt+F2`) y detén el
compositor (`pkill labwc`; init lo relanzará, así que para dejarlo parado
borra `/etc/eclipse/services/labwc.service` o arranca con `desktop=xorg`).

## Personalización

- **Wallpaper**: `lunarbg` es procedural; `LUNARBG_STATIC=1`/`--static`
  desactiva la animación y `LUNARBG_FPS`/`--fps` cambia la cadencia (edita
  el wrapper `/usr/local/bin/eclipse-lunarbg` o la escena en
  `tools/lunarbg/src/scene.rs`). El PNG de
  `/usr/share/backgrounds/eclipse/eclipse-night.png` se regenera fuera de
  un build completo con `cargo test -p xtask dump_wallpaper -- --ignored`.
- **Apariencia completa**: `eclipse-look eclipse|kde|win11` (ver arriba).
- **Retoques sin reconstruir**: labwc lee
  `~/.config/labwc/themerc-override` **encima** del tema, así que ahí puedes
  cambiar colores, bordes o sombras sin tocar el tema ni el build; `xtask` no
  crea ni pisa ese fichero. Recarga con `labwc --reconfigure`.
- **Temas de terceros**: cualquier tema de Openbox 3.6 (los de box-look.org,
  Kaunas, Fluent...) vale tal cual: copia su carpeta a
  `/usr/share/themes/<Nombre>/openbox-3/` y pon ese nombre en el `<name>` de
  `rc.xml`. Ten en cuenta que `eclipse-look` reescribe esa línea, así que un
  cambio de apariencia te lo sobreescribe.
- **Colores del tema**: edita el `themerc` de la apariencia activa
  (`/usr/share/themes/{Win11-Dark,Breeze-Dark,Eclipse-Dark}/openbox-3/themerc`)
  y ejecuta la acción «Recargar labwc» del menú (o `labwc --reconfigure`).
- **Panel**: `lunarbar` no tiene fichero de configuración; su paleta y su
  disposición salen de `/etc/eclipse/look` (`ECLIPSE_LOOK=` lo pisa para una
  ejecución suelta) y el resto de los cambios van en
  `tools/lunarbar/src/` (`draw.rs` para el aspecto, `apps.rs` para el menú)
  y `pkill lunarbar` lo relanza vía init con el nuevo binario.

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
