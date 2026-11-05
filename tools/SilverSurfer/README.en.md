# SilverSurfer

[Español](README.md) · **English**

Eclipse OS's browser for the labwc Wayland session. The engine is the `servo`
0.7 crate (HTML, CSS, JavaScript, WebGL and WebGPU). Audio, video and HLS
streaming are played by GStreamer, through the `media-gstreamer` feature. The
file dialog is `lunarfile`.

## Window

An `xdg-shell` toplevel. The page is a subsurface: EGL when Mesa can create the
context (zink + NVK with `nvidia.wlr_gles2` or `nvidia.wlr_vulkan`; llvmpipe if
there is no card) and, if EGL does not start, a read-back of the software
context copied to `wl_shm`. The chrome — tabs, back, forward, reload and the
bar — is another `wl_shm` subsurface, drawn with tiny-skia, and follows
`/etc/eclipse/look`.

Servo starts with the `multiprocess` feature. Each content process opens its
own DRM file; the kernel gives it a separate GPU context.

Local patches on top of crates.io (until Servo absorbs them):
`vendor/servo-net` (YouTube's HTTP/2 headers) and `vendor/servo-script`
(finalize WebGLBuffer/VAO on close without `assert is_deleted`).

## Building

The build machine needs the development headers, not just the runtime
libraries:

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

Without `libgstreamer1.0-dev`, the media feature does not link. The rest of
the browser does:

```sh
cargo build --release --manifest-path tools/SilverSurfer/Cargo.toml \
  --no-default-features --features engine
```

`cargo xtask` (while assembling the rootfs) builds SilverSurfer on the host:
if `pkg-config --exists gstreamer-1.0` succeeds it uses media; if not, it
warns and builds with `--features engine` only. It copies the binary to
`/usr/bin/silversurfer` when the build succeeds. It does not go to `/bin`:
that directory goes into the boot image with a 16 MiB cap and this binary is
larger, so the copy was being dropped and the booted system had no browser.
`/usr/bin` is copied whole with the rest of the desktop. The desktop installs
`eclipse-browser` (prefers SilverSurfer, otherwise Firefox),
`silversurfer.desktop` and the default `http`/`https` handler. The image
carries the runtime plugins (`gstreamer1.0-plugins-*` / `gst-plugins-*`).

The unit tests and benchmarks do not start Servo. They measure the chrome, the
address bar, the lunarfile dialog and copying a frame to wl_shm:

```sh
cargo test --manifest-path tools/SilverSurfer/Cargo.toml --no-default-features
cargo bench --manifest-path tools/SilverSurfer/Cargo.toml \
  --no-default-features --bench chrome
```

## Usage

```sh
silversurfer
silversurfer --fresh
silversurfer https://example.com
silversurfer /path/page.html
```

Without a URL it restores the tabs from
`$XDG_DATA_HOME/silversurfer/session.json` (or `~/.local/share/…`). `--fresh`
ignores that session. `--private` opens a private window (no session; on close
it deletes that instance's cookies/storage/cache).

| Key | Action |
|---|---|
| Alt+Left / Alt+Right | Back / forward |
| F5 or Ctrl+R | Reload |
| Ctrl+L | Address bar (omnibox: URL or search) |
| Ctrl+T / Ctrl+W | New tab / close |
| Ctrl+Tab / Ctrl+Shift+Tab | Next / previous tab |
| Ctrl+1 … Ctrl+8 | Go to tab N |
| Ctrl+O | `lunarfile --pick` and open the file |
| Ctrl+Shift+P | Private window (another process) |
| Ctrl+Shift+Delete | Delete cookies and site data |
| Ctrl++ / Ctrl+- / Ctrl+0 | Page zoom / reset |
| Ctrl+F | Find in page (`window.find`) |
| Ctrl+D | Add or remove bookmark |
| Ctrl+H / Ctrl+B | History / bookmarks |
| Ctrl+S | Download the current URL (`lunarfile --save` + curl) |
| Ctrl+J | Download list |
| F11 / Escape | Full screen / leave |
| Ctrl+N | New window (another process) |
| Drag a tab | Reorder |
| Right click (page) | Context menu |
| Tab (in the bar) | Accept the history suggestion |
| Ctrl+C / Ctrl+X / Ctrl+V (in the bar) | Copy / cut the selection (or the whole address), paste |
| Ctrl+A, Shift+arrows, Shift+Home/End (in the bar) | Select |
| Ctrl+← / Ctrl+→, Ctrl+Backspace / Ctrl+Del (in the bar) | Jump / delete by URL chunks |
| Click / double click / drag (in the bar) | The first click selects the address; then cursor, word or selection |
| Ctrl+Shift+L | Save the visible form's login |

The bar searches with DuckDuckGo if the text does not look like an address
(`SILVERSURFER_SEARCH` changes the template; use `{query}` or `{}`). Visits are
saved in `$XDG_DATA_HOME/silversurfer/history.json`, bookmarks in
`bookmarks.json` and the download log in `downloads.json`. `about:history`,
`about:bookmarks` and `about:downloads` open local lists. Navigating to a
PDF/ZIP/… intercepts the load and asks for a destination with lunarfile (Servo
0.7 exposes no native download hook). The download is done by curl with the
cookies Servo has for that URL (passed on standard input, never on the command
line), the same User-Agent and only the page's origin as the Referer, so links
that need a signed-in session work too.
The default User-Agent mimics desktop Firefox (with `Gecko/` and `Firefox/`,
plus `Servo/0.7`) so sites like Google do not serve the degraded shell;
`SILVERSURFER_UA` replaces it entirely.
Each tab shows the page's favicon (reduced to 16×16 and scaled with the
output). The window title follows the active tab; while loading, the tab shows
`…` and the bar a scheme badge (`s` https, `!` http, `/` file).
The chrome is painted at the output's scale (native HiDPI), with vector icons,
and reloads the palette if `/etc/eclipse/look` changes.
The cursor follows the page (hand on links, I-beam on text, resize…) and the
chrome (hand on buttons and tabs, I-beam in the bar) via `wp_cursor_shape_v1`;
without that protocol the compositor's stays.
Dead keys (´ + a → á, ¨ + u → ü, ~ + n → ñ…) go through xkb compose with the
locale's table (`LC_ALL`/`LC_CTYPE`/`LANG`); if the image has no
`/usr/share/X11/locale`, there is a minimal built-in table for Latin accents.
The chrome draws Latin-1 (ISO 8859-1), so "Nueva pestaña" and URLs with
accents look right.
The clipboard is the system one: `arboard` on wlr/ext data-control (labwc),
with X11 and an in-memory text as fallback. The page's fields,
`navigator.clipboard` and the bar use it.
There is also a primary selection: what is selected in the bar or on the page
(on mouse release; never a password field) can be pasted with the middle
button in another app, and the middle button on the bar or on a page text field
pastes the system's primary selection.
The `GL` / `SW` badge tells whether the page goes through EGL or a software
blit; in a private window `P` appears as well.
`about:privacy`, `about:cookies` and `about:clear` cover the data UI.
Lite ad blocking (hosts/paths + CSS hide) is on by default;
`SILVERSURFER_ADBLOCK=0` turns it off. See `about:filters`.
Passwords: a local store encrypted with ChaCha20-Poly1305 (`passwords.json`,
a random key kept apart in `passwords.key`, both 0600; old obfuscated stores
migrate by themselves when opened); autofill on loading the origin;
`about:passwords` lists entries. Disabled in `--private`.
The home page is a blank canvas with the Eclipse OS logo (palette from
`/etc/eclipse/look`). The canvas / WebGL / WebGPU / WebAudio / MP4 / HLS smokes
live in `smoke.html` (`smoke_url()`).

## React / SPAs (Servo 0.7)

The **full embedder surface** (prefs + polyfills + reveal):

- Experimental DOM prefs (IO, Web Animations, FontFace, adoptedStyleSheets,
  IndexedDB, clipboard, viewport, Entries, OffscreenCanvas, CookieStore,
  Credentials, Permissions, module preload, …). Service Workers **off**.
- Crate features: `brotli-compression-stream`. **No `webcrypto`**: in
  servo-script 0.7 it pulls in `aes-gcm ^0.11` (only RCs in indexes before
  2026-06, so resolution fails) and pinned prereleases (`rsa`, `x448`,
  `ed448-goldilocks`). Without it the engine exposes no `crypto`; the
  userscript creates a stub with `getRandomValues` / `randomUUID` (no
  `subtle`).
- Userscripts: `AbortController`/`AbortSignal` (+ `abort`/`timeout`/`any`),
  idle/microtask/scheduler, `structuredClone`, the `crypto` stub,
  `Object.hasOwn`, `replaceAll`/`replaceChildren`, no-op View Transitions,
  Intersection/ResizeObserver and CE stubs **only if missing**, GitHub reveal
  (a one-shot CSS sheet + forced `matchMedia(prefers-reduced-motion)`, without
  fighting GSAP with `setInterval`).
- Page console → stderr via `show_console_message`
  (`SILVERSURFER_CONSOLE=warn|error|log|off`). An optional probe on load
  (`SILVERSURFER_PROBE=/path/script.js`).
- A `React / SPA APIs` smoke in `smoke.html`.

**Servo's ceiling (not 100 % Firefox):** real Custom Elements / Shadow DOM,
Service Workers, modern streams/fetch, signed-in GitHub apps / hero video. The
`servo` 0.7 crate is the latest on crates.io.

## Media and limits

With `media-gstreamer`, `<audio>` / `<video>` and simple HLS playlists
(`*.m3u8` with MPEG-TS segments) are played by GStreamer (`GstHLSDemux2`). The
HLS smoke uses an Apple media playlist (bipbop) and only loads it on pressing
"Probar HLS", so as not to trigger `Player error` / `Invalid playlist` when the
page opens.

Media Source Extensions (`MediaSource`, `SourceBuffer`) are implemented in the
vendored Servo crates (see `vendor/servo-media-gstreamer/README.eclipse.md`):
YouTube and other DASH players append MP4 (H.264/AAC) or WebM (VP9/Opus)
fragments and GStreamer demuxes and plays them, with seeking.
`isTypeSupported` answers according to the installed GStreamer decoders, so a
system without `avdec_h264` or `vp9dec` gets the other variant or none. There
is no `changeType` between containers (MP4↔WebM) nor EME (DRM video). If a
page asks for native HLS with an unusual or encrypted master, the demuxer may
fail; the smoke marks it red instead of retrying in a loop.

`<input type=file>` opens `lunarfile` too. Escape in the picker cancels.

Startup: the log includes `session_ms` until Servo is ready. With
`SILVERSURFER_REQUIRE_EGL=1` it fails if there is no EGL (no software
fallback). With zink (`GALLIUM_DRIVER` or `MESA_LOADER_DRIVER_OVERRIDE` =
`zink`, Eclipse OS's GPU sessions on NVK) the EGL context is created and Servo
paints on the GPU, but what is presented never shows: the page stayed empty
under the `GL` badge. There the page is rendered into Servo's off-screen FBO
and read back into the toplevel's `wl_shm`, as in software (`paint=EGL→wl_shm`
in the log). `SILVERSURFER_PRESENT=egl` or `=shm` forces one path or the
other. The first three frames read back are summarised in the log (`fotograma
N leído WxH medio=#rrggbb opacos=P%`): if they show up and the window does not
change, the failure is in composition; if they do not show up or the tab never
gets past "Nueva pestaña", the page is not loading. `SILVERSURFER_MULTIPROCESS=0`
keeps script and layout in the same process to rule out the content process
(`procesos=uno` in the log).
`SILVERSURFER_SOFTWARE=1` does the opposite: it skips EGL and paints the page
in software→wl_shm, the path Eclipse OS takes with labwc on pixman. There the
toplevel uses two buffers: pixman does not release a `wl_shm` until it gets
another, and with just one the page was not copied again after the first
commit. On that path the page is read with `glReadPixels(GL_BGRA)` straight
into a reused buffer (the same bytes as `wl_shm` ARGB8888, without reordering
channels), compared with the previous frame and only the rows that changed are
copied and marked as damage; if nothing changed there is no commit. Each
commit asks for a `wl_surface.frame` and the next frame waits for that
callback, so the page is not painted faster than the monitor nor while the
window is not visible. `SILVERSURFER_FRAME_STATS=1` prints fps, read-back and
copy ms and the % of damaged rows every 120 frames.
The chrome skips the Wayland commit if the content did not change; only the
active tab drives frame presentation.
