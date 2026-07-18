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
| Wallpaper | `/usr/share/backgrounds/eclipse/eclipse-night.png` | Escena nocturna (cielo degradado, estrellas, luna, montañas y el disco de Eclipse). Se **renderiza en build** con un encoder PNG propio, sin dependencias. |
| Tema de ventanas | `/usr/share/themes/Eclipse-Dark/openbox-3/themerc` | Tema openbox-3 oscuro que labwc aplica a bordes de ventana, menús y OSD. |
| Config labwc | `/root/.config/labwc/rc.xml` | Tema `Eclipse-Dark`, esquinas redondeadas, 4 escritorios y atajos de teclado. |
| Menú de escritorio | `/root/.config/labwc/menu.xml` | Clic derecho en el fondo: terminal, editor, monitor, recargar y salir. |
| Entorno de sesión | `/root/.config/labwc/environment` | Cursor Adwaita y `GTK_THEME=Adwaita:dark`. |
| Autoarranque | `/root/.config/labwc/autostart` | Lanza `swaybg` (wallpaper), `foot` (terminal) y `waybar` (panel, el último). Cada cliente está protegido con `command -v`: si falta, se anota en el log y la sesión sigue. |
| Panel | `/root/.config/waybar/{config,style.css}` | Barra inferior: lanzador + barra de tareas a la izquierda; CPU, memoria y reloj a la derecha. |
| GTK 3/4 | `/root/.config/gtk-{3.0,4.0}/settings.ini` | Modo oscuro por defecto para aplicaciones GTK. |
| Terminal | `/root/.config/foot/foot.ini` | Paleta violeta oscura a juego con el escritorio. |
| Lanzador | `/usr/local/bin/labwc` | Wrapper que garantiza `XDG_RUNTIME_DIR` y el tema de cursor aunque `login(1)` haya limpiado el entorno. |

## Paquetes de runtime

El núcleo y el rootfs no incluyen los binarios Wayland; se instalan desde
Alpine. Todos son opcionales — la sesión degrada con elegancia si falta
alguno:

```sh
apk add labwc waybar foot swaybg font-dejavu adwaita-icon-theme
```

- `labwc` — el compositor.
- `swaybg` — pinta el wallpaper (sin él, escritorio negro con color de respaldo).
- `waybar` — el panel inferior (sin él, no hay barra pero todo funciona).
- `foot` — terminal Wayland.
- `font-dejavu` — tipografía usada por tema, panel y menús.
- `adwaita-icon-theme` — tema de cursor e iconos (sin él no se ve el puntero
  con cursor software).

## Atajos de teclado

| Atajo | Acción |
|---|---|
| `Super+Enter` / `Alt+Enter` | Abrir terminal (`foot`) |
| `Super+Espacio` | Menú de escritorio |
| `Alt+Tab` | Cambiar de ventana |
| `Alt+F4` | Cerrar ventana |
| `Super+↑` | Maximizar / restaurar |
| `Super+←` / `Super+→` | Anclar a media pantalla |
| `Super+1..4` | Ir al escritorio N |
| `Super+Shift+1..4` | Mover ventana al escritorio N |

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
