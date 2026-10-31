# SilverSurfer

Navegador de Eclipse OS para la sesión Wayland de labwc. El motor es el
crate `servo` 0.7 (HTML, CSS, JavaScript, WebGL y WebGPU). El audio, el
vídeo y el streaming HLS los reproduce GStreamer, a través del feature
`media-gstreamer`. El diálogo de archivos es `lunarfile`.

## Ventana

Un toplevel `xdg-shell`. La página es una subsuperficie: EGL cuando Mesa
puede crear el contexto (zink + NVK con `nvidia.wlr_gles2` o
`nvidia.wlr_vulkan`; llvmpipe si no hay tarjeta) y, si EGL no arranca, una
lectura del contexto de software copiada a `wl_shm`. El cromo —pestañas,
atrás, adelante, recargar y la barra— es otra subsuperficie `wl_shm`,
dibujada con tiny-skia, y sigue `/etc/eclipse/look`.

Servo arranca con el feature `multiprocess`. Cada proceso de contenido abre
su propio fichero DRM; el kernel le da un contexto de GPU distinto.

## Construir

En la máquina que compila hacen falta las cabeceras de desarrollo, no solo
las bibliotecas de ejecución:

```sh
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-libav \
  libwayland-dev libxkbcommon-dev libegl1-mesa-dev \
  clang cmake python3 pkg-config
```

```sh
cargo build --release --manifest-path tools/SilverSurfer/Cargo.toml
```

Sin `libgstreamer1.0-dev`, el feature de medios no enlaza. El resto del
navegador sí:

```sh
cargo build --release --manifest-path tools/SilverSurfer/Cargo.toml \
  --no-default-features --features engine
```

`cargo xtask` (al armar el rootfs) compila SilverSurfer en el host: si
`pkg-config --exists gstreamer-1.0` tiene éxito usa medios; si no, avisa y
compila solo con `--features engine`. Copia el binario a `/bin/silversurfer`
cuando el build sale bien. El escritorio instala `eclipse-browser` (prefer
SilverSurfer, si no Firefox), `silversurfer.desktop` y el handler
`http`/`https` por defecto. La imagen lleva los plugins de ejecución
(`gstreamer1.0-plugins-*` / `gst-plugins-*`).

Las pruebas de unidad y los benchmarks no arrancan Servo. Miden el cromo, la
barra de direcciones, el diálogo de lunarfile y la copia de un fotograma a
wl_shm:

```sh
cargo test --manifest-path tools/SilverSurfer/Cargo.toml --no-default-features
cargo bench --manifest-path tools/SilverSurfer/Cargo.toml \
  --no-default-features --bench chrome
```

## Uso

```sh
silversurfer
silversurfer --fresh
silversurfer https://example.com
silversurfer /ruta/pagina.html
```

Sin URL restaura las pestañas de
`$XDG_DATA_HOME/silversurfer/session.json` (o `~/.local/share/…`).
`--fresh` ignora esa sesión. `--private` abre una ventana privada (sin
sesión; al cerrar borra cookies/almacenamiento/caché de esa instancia).

| Tecla | Acción |
|---|---|
| Alt+Izquierda / Alt+Derecha | Atrás / adelante |
| F5 o Ctrl+R | Recargar |
| Ctrl+L | Barra de dirección (omnibox: URL o búsqueda) |
| Ctrl+T / Ctrl+W | Pestaña nueva / cerrar |
| Ctrl+Tab / Ctrl+Shift+Tab | Pestaña siguiente / anterior |
| Ctrl+1 … Ctrl+8 | Ir a la pestaña N |
| Ctrl+O | `lunarfile --pick` y abre el fichero |
| Ctrl+Shift+P | Ventana privada (otro proceso) |
| Ctrl+Shift+Delete | Borrar cookies y datos de sitio |

La barra busca con DuckDuckGo si el texto no parece una dirección
(`SILVERSURFER_SEARCH` cambia la plantilla; usa `{query}` o `{}`).
El User-Agent por defecto imita Firefox de escritorio (con `Gecko/` y
`Firefox/`, más `Servo/0.7`) para que sitios como Google no sirvan la
cáscara degradada; `SILVERSURFER_UA` lo sustituye entero.
El título de la ventana sigue la pestaña activa; mientras carga, el tab
muestra `…` y la barra un distintivo de esquema (`s` https, `!` http, `/` file).
El cromo se pinta a la escala del output (HiDPI nativo), con iconos
vectoriales, y recarga la paleta si cambia `/etc/eclipse/look`.
El distintivo `GL` / `SW` indica si la página va por EGL o por blit
software; en ventana privada aparece además `P`.
`about:privacy`, `about:cookies` y `about:clear` cubren la UI de datos.
La home es un lienzo en blanco con el logo de Eclipse OS
(paleta de `/etc/eclipse/look`). Los smokes de canvas / WebGL / WebGPU /
WebAudio / MP4 / HLS viven en `smoke.html` (`smoke_url()`).

## React / SPAs (Servo 0.7)

El embutidor activa prefs experimentales que los bundles React tocan
(`IntersectionObserver`, Web Animations, `FontFace`, `adoptedStyleSheets`,
IndexedDB, visual viewport, container queries, …) e inyecta userscripts
mínimos: polyfill de `AbortController` / `requestIdleCallback` /
`queueMicrotask` si faltan, y un reveal de `opacity:1` gated a
`github.com` (el landing SSR deja el hero en `opacity:0` hasta GSAP).

Esto acerca sitios como la homepage de GitHub a “usable a primera vista”.
**No** iguala Firefox: Custom Elements / Shadow DOM robustos, Service
Workers, streams modernos y la hidratación React completa siguen siendo
techo del motor. Service Workers se dejan apagados a propósito (un SW
a medias rompe más que su ausencia). Crate `servo` 0.7 es lo último en
crates.io; un bump no está disponible aún.

## Medios y límites

Con `media-gstreamer`, `<audio>` / `<video>` y listas HLS simples (`*.m3u8`
con segmentos MPEG-TS) las reproduce GStreamer (`GstHLSDemux2`). El smoke HLS
usa una lista media de Apple (bipbop) y solo la carga al pulsar «Probar HLS»,
para no disparar `Player error` / `Invalid playlist` al abrir la página.

No esperes YouTube ni DASH/MSE adaptativo por este camino: el reproductor de
YouTube no alimenta un `<video src="…m3u8">` a GStreamer. Si una página pide
HLS nativo con un master raro o cifrado, el demuxer puede fallar; el smoke lo
marca en rojo en lugar de reintentar en bucle.

`<input type=file>` también abre `lunarfile`. Escape en el selector cancela.
