# eclipse-labwc

Clon de **[labwc 0.8](https://github.com/labwc/labwc)** sobre **Smithay 0.7 nativo**, integrado en **Eclipse OS** (kernel + filesystem) — sin `wayland-proto`, sin `sidewind`, sin libs heredadas.

## Estado de build

✅ **Compila limpio para `x86_64-unknown-linux-musl`** (`cargo check`, 0 errores).
🔧 **Listo para target Eclipse**: añadir el `[patch.crates-io]` indicado en `eclipse-apps/Cargo.toml` y compilar con `RUSTFLAGS="--cfg rustix_use_libc"` + `-Z build-std=core,alloc,std`.

## Stack

| Capa | Crate / Subsistema |
|---|---|
| Compositor framework | **`smithay = "0.7"`** (`desktop`, `wayland_frontend`, `renderer_pixman`, `xwayland`, `backend_drm`, `backend_libinput`, `backend_session`, `backend_winit`) |
| Event loop | **`calloop = "0.14"`** |
| Teclado | **`xkbcommon = "0.8"`** |
| DRM/KMS | **`drm = "0.14"`** + **`gbm = "0.18"`** (rust-drm, sin libdrm.so) |
| Input | **`input = "0.9"`** (libinput-rs sobre `/dev/input/event*`) |
| XWayland | **`smithay::xwayland`** + binario `eclipse-xwayland` |
| Render SSD | **`tiny-skia = "0.11"`** (no_std-float, software) |
| Buffers | **`pixman = "0.2"`** (renderer_pixman) |
| Errores / log | **`anyhow`** + **`tracing`** |
| Kernel Eclipse | **`eclipse-syscall`** (solo `cfg(target_os="eclipse")`) |
| Libc Eclipse | **`eclipse-relibc`** (solo `cfg(target_os="eclipse")`, vía patch) |
| Backend dev | **`winit = "0.30"`** (target Linux host) |

> **Sin** `wayland-proto`, sin `sidewind`, sin `seatd/libseat`. La sesión la implementa `EclipseSession` en `src/backend/session.rs` (abre `/dev/dri/card*` y `/dev/input/event*` directamente con la libc Eclipse).

## Funcionalidades de labwc replicadas

- ✅ `rc.xml` — core/decoration/gap, theme, focus model, keybind, mousebind, autostart, windowRules
- ✅ `themerc` — colores, bordes, fuentes (Openbox 3 format)
- ✅ `menu.xml` — root menu, submenús, separadores
- ✅ Stacking WM, focus next/prev (Alt+Tab), raise on focus
- ✅ Server-side decorations vía `xdg-decoration` + render real con tiny-skia
- ✅ `xdg-shell` (toplevel + popup) — Smithay
- ✅ **`PointerGrab` para Move/Resize** interactivos (`src/grabs.rs`)
- ✅ `wlr-layer-shell` (panels, docks) — handler stub
- ✅ `wl_seat` (keyboard + pointer + touch)
- ✅ `wl_output` + `xdg-output`
- ✅ `wl_data_device` + `primary-selection`
- ✅ `wp_viewporter`
- 🔧 XWayland — `XwmHandler` implementado, `start_xwayland()` con TODO de `XWayland::spawn()`

## Build

### Host Linux (musl o gnu)

```sh
cd /app/eclipse-apps/eclipse-labwc
cargo check                            # ✅ pasa limpio
cargo build --release                  # genera target/.../eclipse-labwc
```

(El crate aísla su propio `[workspace]` para no arrastrar los demás miembros del workspace `eclipse-apps`.)

### Target Eclipse OS (DRM/KMS real)

1. Descomenta el bloque `[patch.crates-io]` al final de `eclipse-apps/Cargo.toml`.
2. Asegúrate que existen los forks en `eclipse-apps/vendor/{libc,wayland-sys,bitflags,errno,downcast-rs}` (los crea `populate_headers.sh` o build.sh upstream).
3. Compila:

```sh
cd /app/eclipse-apps/eclipse-labwc
RUSTFLAGS="--cfg rustix_use_libc" \
cargo +nightly build -p eclipse-labwc --release \
    --target x86_64-unknown-eclipse \
    -Z build-std=core,alloc,std
```

## Configuración

Mismas rutas que labwc upstream:

- `/etc/labwc/rc.xml` (o `/usr/share/labwc/rc.xml`)
- `/etc/labwc/themes/<NAME>/openbox-3/themerc` (o `/usr/share/themes/...`)
- `/etc/labwc/menu.xml`

```sh
sudo cp examples/rc.xml      /etc/labwc/rc.xml
sudo cp examples/menu.xml    /etc/labwc/menu.xml
sudo cp -r examples/themes/Default /etc/labwc/themes/Default
```

## Estructura

```
src/
├── main.rs                  ← entry
├── lib.rs                   ← re-exports
├── server.rs                ← bootstrap (Display + EventLoop + LabwcState)
├── state.rs                 ← LabwcState con todos los *State de Smithay 0.7
├── grabs.rs                 ← PointerGrab Move + Resize ✅
├── render.rs                ← Render SSD con tiny-skia (titlebar+bordes+botones) ✅
├── handlers/
│   ├── compositor.rs        ← CompositorHandler
│   ├── xdg_shell.rs         ← XdgShellHandler (toplevel + popup + grabs)
│   ├── xdg_decoration.rs    ← SSD obligatorio según rc.xml
│   ├── layer_shell.rs       ← zwlr_layer_shell_v1 (stub)
│   ├── shm.rs               ← Buffer/Shm/Seat/DataDevice/Selection/Output/Viewporter
│   └── xwayland.rs          ← XwmHandler (start_xwayland TODO)
├── backend/
│   ├── drm.rs               ← DRM/KMS via smithay::backend::drm + libinput
│   ├── session.rs           ← EclipseSession ✅ (sin seatd/libseat)
│   └── winit.rs             ← host Linux dev backend
├── config.rs                ← parser rc.xml (XML mínimo)
├── theme.rs                 ← parser themerc
├── menu.rs                  ← parser menu.xml + overlay UI
├── key.rs                   ← keybindings parse + match + evdev→xkb
├── actions.rs               ← enum Action
├── view.rs                  ← Stack (z-order)
├── ssd.rs                   ← layout botones + hit-test resize edges
├── xwayland_mgr.rs          ← wrapper de start_xwayland
└── examples/                ← rc.xml, menu.xml, themes/Default/openbox-3/themerc
```

## Notas Eclipse OS específicas

1. **Sesión sin seatd**: `EclipseSession` (`src/backend/session.rs`) implementa `smithay::backend::session::Session` abriendo `/dev/dri/card0` y `/dev/input/event*` directamente con `libc::open` (que en target Eclipse es `eclipse-relibc::open` → syscall del kernel).
2. **udev fuera**: `enumerate_drm_devices()` y `enumerate_input_devices()` listan los directorios `/dev/dri` y `/dev/input` con `std::fs::read_dir` (sobre eclipse-relibc).
3. **rustix backend libc**: necesario forzar `--cfg rustix_use_libc` para que rustix use libc en lugar de syscalls Linux directos.
4. **XWayland**: el binario es `eclipse-xwayland` instalado en `/usr/bin`; `start_xwayland()` lo lanza con `XWayland::spawn()` (TODO completar firma exacta).
5. **getrandom**: `eclipse-relibc` lo expone via `SYS_GETRANDOM`.
6. **Spawn**: `state::spawn(cmd)` usa `eclipse_syscall::call::spawn_command` en target Eclipse, `std::process::Command` en target Linux dev.

## Roadmap

- [ ] Completar `start_xwayland()` con la firma real de `XWayland::spawn()` en Smithay 0.7.
- [ ] Implementar `layer_map_for_output().map_layer(...)` en `handlers/layer_shell.rs`.
- [ ] Render real frame loop (DRM page-flip + GbmAllocator + GlesRenderer en `backend/drm.rs`).
- [ ] Multi-output (varios monitores).
- [ ] Texto de titlebar con `cosmic-text` o `fontdue`.
- [ ] DnD entre clientes Wayland y XWayland.

## Licencia

MIT — igual que labwc y Eclipse OS.
