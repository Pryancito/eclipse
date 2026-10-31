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
`--fresh` ignora esa sesión.

| Tecla | Acción |
|---|---|
| Alt+Izquierda / Alt+Derecha | Atrás / adelante |
| F5 o Ctrl+R | Recargar |
| Ctrl+L | Barra de dirección (omnibox: URL o búsqueda) |
| Ctrl+T / Ctrl+W | Pestaña nueva / cerrar |
| Ctrl+Tab / Ctrl+Shift+Tab | Pestaña siguiente / anterior |
| Ctrl+1 … Ctrl+8 | Ir a la pestaña N |
| Ctrl+O | `lunarfile --pick` y abre el fichero |
| Ctrl++ / Ctrl+- / Ctrl+0 | Zoom página / restablecer |
| Ctrl+F | Buscar en la página (`window.find`) |
| Ctrl+D | Añadir o quitar marcador |
| Ctrl+H / Ctrl+B | Historial / marcadores |
| Ctrl+S | Descargar URL actual (`lunarfile --save` + curl) |
| Ctrl+J | Lista de descargas |
| F11 / Escape | Pantalla completa / salir |
| Tab (en la barra) | Aceptar sugerencia del historial |

La barra busca con DuckDuckGo si el texto no parece una dirección
(`SILVERSURFER_SEARCH` cambia la plantilla; usa `{query}` o `{}`).
Las visitas se guardan en
`$XDG_DATA_HOME/silversurfer/history.json`, los marcadores en
`bookmarks.json` y el registro de descargas en `downloads.json`.
`about:history`, `about:bookmarks` y `about:downloads` abren listas
locales. Navegar a PDF/ZIP/… intercepta la carga y pide destino con
lunarfile (Servo 0.7 no expone un hook de descarga nativo). El título de la ventana sigue la pestaña activa; mientras carga,
el tab muestra `…` y la barra un distintivo de esquema (`s` https, `!`
http, `/` file). El cromo se pinta a la escala del output (HiDPI nativo),
con iconos vectoriales, y recarga la paleta si cambia `/etc/eclipse/look`.
El distintivo `GL` / `SW` indica si la página va por EGL o por blit
software; la home lista smokes locales (canvas, WebGL, WebGPU, WebAudio)
y remotas (MP4/HLS).

`<input type=file>` también abre `lunarfile`. Escape en el selector cancela.
