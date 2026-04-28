# Eclipse-LabWC — PRD

## Problem statement
Construir un clon de labwc 0.8 adaptado a Eclipse OS (kernel custom de Pryancito), basado **únicamente** en Smithay nativo, sin `wayland-proto` ni `sidewind`. Mantener todas las features de labwc (xdg-shell, xwayland, drm, layer-shell, rc.xml, themerc, menu.xml, keybindings, SSD).

## User choices
- Smithay 0.7 nativo (sin wayland-proto, sin sidewind).
- Backends: DRM/KMS, libinput, winit (host dev), XWayland.
- Target real: `x86_64-unknown-linux-musl` (lo que el usuario va a montar) con paths a Eclipse OS via `[patch.crates-io]`.
- Min libs sys posibles: `eclipse-syscall` + `eclipse-relibc` solo en target Eclipse.

## Implementado (Apr 2026)

**Estructura**: 24 archivos en `eclipse-apps/eclipse-labwc/` cubriendo:
- `state.rs` — `LabwcState` con todos los `*State` de Smithay 0.7
- `handlers/` (7 archivos) — Compositor, XdgShell, XdgDecoration, LayerShell, Shm/Seat/DataDevice/Selection/Output/Viewporter, XWayland
- `backend/` (3 archivos) — DRM, Session (sin seatd), Winit
- `grabs.rs` — `PointerGrab` real para Move/Resize ✅
- `render.rs` — render SSD con tiny-skia (titlebar+bordes+botones+botones redondeados) ✅
- `config.rs` / `theme.rs` / `menu.rs` — parsers XML para rc.xml, themerc, menu.xml
- `key.rs` / `actions.rs` — keybindings con parser estilo labwc (`W-q`, `A-Tab`, etc)
- `view.rs` / `ssd.rs` — stacking WM + hit-test SSD
- `examples/` — rc.xml, menu.xml, themes/Default

**Build status**: `cargo check --target x86_64-unknown-linux-musl` ✅ **0 errores, 1 warning** menor.

**Patches workspace**: bloque `[patch.crates-io]` añadido (comentado por defecto) en `eclipse-apps/Cargo.toml` con instrucciones para target Eclipse.

## TODOs explícitos en el código
- `start_xwayland()`: adaptar firma a `XWayland::spawn()` en Smithay 0.7.
- `layer_shell.rs::new_layer_surface`: usar `layer_map_for_output()`.
- `backend/drm.rs::run`: completar GbmAllocator + GlesRenderer + page-flip loop (patrón anvil/udev.rs).

## Build commands

### Host Linux musl (validación)
```sh
cd /app/eclipse-apps/eclipse-labwc && cargo check
```

### Target Eclipse OS (real)
```sh
cd /app/eclipse-apps/eclipse-labwc
RUSTFLAGS="--cfg rustix_use_libc" \
cargo +nightly build --release \
    --target x86_64-unknown-eclipse \
    -Z build-std=core,alloc,std
# Requiere descomentar [patch.crates-io] en eclipse-apps/Cargo.toml
# y tener los forks en eclipse-apps/vendor/.
```
