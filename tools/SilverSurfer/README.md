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

Parches locales sobre crates.io (hasta que Servo los absorba):
`vendor/servo-net` (cabeceras HTTP/2 de YouTube) y `vendor/servo-script`
(finalize WebGLBuffer/VAO al cerrar sin `assert is_deleted`).

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
| Ctrl++ / Ctrl+- / Ctrl+0 | Zoom página / restablecer |
| Ctrl+F | Buscar en la página (`window.find`) |
| Ctrl+D | Añadir o quitar marcador |
| Ctrl+H / Ctrl+B | Historial / marcadores |
| Ctrl+S | Descargar URL actual (`lunarfile --save` + curl) |
| Ctrl+J | Lista de descargas |
| F11 / Escape | Pantalla completa / salir |
| Ctrl+N | Ventana nueva (otro proceso) |
| Arrastrar pestaña | Reordenar |
| Clic derecho (página) | Menú contextual |
| Tab (en la barra) | Aceptar sugerencia del historial |
| Ctrl+Shift+L | Guardar login del formulario visible |

La barra busca con DuckDuckGo si el texto no parece una dirección
(`SILVERSURFER_SEARCH` cambia la plantilla; usa `{query}` o `{}`).
Las visitas se guardan en
`$XDG_DATA_HOME/silversurfer/history.json`, los marcadores en
`bookmarks.json` y el registro de descargas en `downloads.json`.
`about:history`, `about:bookmarks` y `about:downloads` abren listas
locales. Navegar a PDF/ZIP/… intercepta la carga y pide destino con
lunarfile (Servo 0.7 no expone un hook de descarga nativo).
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
El bloqueo de anuncios lite (hosts/paths + CSS hide) está activo por
defecto; `SILVERSURFER_ADBLOCK=0` lo apaga. Ver `about:filters`.
Contraseñas: almacén local ofuscado (`passwords.json`); autocompletado al
cargar el origen; `about:passwords` lista entradas. Desactivado en `--private`.
La home es un lienzo en blanco con el logo de Eclipse OS
(paleta de `/etc/eclipse/look`). Los smokes de canvas / WebGL / WebGPU /
WebAudio / MP4 / HLS viven en `smoke.html` (`smoke_url()`).

## React / SPAs (Servo 0.7)

Superficie **completa del embutidor** (prefs + polyfills + reveal):

- Prefs DOM experimentales (IO, Web Animations, FontFace, adoptedStyleSheets,
  IndexedDB, clipboard, viewport, Entries, OffscreenCanvas, CookieStore,
  Credentials, Permissions, module preload, …). Service Workers **off**.
- Features del crate: `brotli-compression-stream`. **Sin `webcrypto`**: en
  servo-script 0.7 arrastra `aes-gcm ^0.11` (solo RCs en índices anteriores a
  2026-06, así que la resolución falla) y prereleases fijadas (`rsa`, `x448`,
  `ed448-goldilocks`). Sin ella no existe el global `crypto` (ni
  `getRandomValues` ni `subtle`); el polyfill de `randomUUID` se omite.
- Userscripts: `AbortController`/`AbortSignal` (+ `abort`/`timeout`/`any`),
  idle/microtask/scheduler, `structuredClone`, `randomUUID`, `Object.hasOwn`,
  `replaceAll`/`replaceChildren`, View Transitions no-op, stubs de
  Intersection/ResizeObserver y CE **solo si faltan**, reveal GitHub
  (`opacity`/`visibility`/`inert`).
- Smoke `React / SPA APIs` en `smoke.html`.

**Techo Servo (no 100 % Firefox):** Custom Elements / Shadow DOM reales,
Service Workers, streams/fetch moderno, apps GitHub logueadas. Crate
`servo` 0.7 es lo último en crates.io.

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

Arranque: el log incluye `session_ms` hasta tener Servo listo. Con
`SILVERSURFER_REQUIRE_EGL=1` falla si no hay EGL (sin fallback software).
El cromo omite el commit Wayland si el contenido no cambió; solo la
pestaña activa dispara el present de frames.
