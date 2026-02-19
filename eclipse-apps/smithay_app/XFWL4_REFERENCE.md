# Smithay App — Referencia de diseño basada en xfwl4

Este documento describe cómo usar **xfwl4** como referencia para mejorar smithay_app, mapeando conceptos y patrones de xfwl4 a equivalentes compatibles con Eclipse OS (no_std, framebuffer, IPC).

> **Nota**: xfwl4 no puede ejecutarse en Eclipse; depende de Linux, Wayland, DRM, glib, etc. Aquí se extraen ideas y patrones que sí son portables.

---

## 1. Arquitectura de xfwl4 (referencia)

```
xfwl4/
├── main.rs           → Event loop, backends (udev/winit/x11)
├── state.rs          → Estado global (ventanas, focus, outputs, workspaces)
├── focus.rs          → Objetos que pueden recibir focus (teclado, puntero)
├── cycle.rs          → Lógica de ciclado de ventanas (Alt+Tab)
├── input_handler.rs  → Procesamiento de teclas, keybindings, acciones
├── workspaces.rs     → Workspaces, layout
├── shell/            → Decoraciones SSD, ventanas, layer surfaces
├── cursor.rs         → Cursor del ratón
├── render.rs         → Renderizado
├── config/           → Configuración teclado, ratón, etc.
├── handlers/         → Handlers Wayland (xdg, seat, output, …)
├── backend/          → udev, winit, x11
└── ui/               → GTK, tabwin
```

---

## 2. Mapeo xfwl4 → smithay_app

| xfwl4 | smithay_app (actual) | Acción sugerida |
|-------|----------------------|-----------------|
| `state.rs` (Xfwl4State) | Todo en `main.rs` | Extraer `CompositorState` con ventanas, focus, workspaces |
| `focus.rs` (KeyboardFocusTarget) | `InputState.dragging_window` | Añadir `focused_window: Option<usize>` |
| `cycle.rs` | No existe | Añadir ciclado Alt+Tab con índice de ventanas |
| `input_handler.rs` (KeyAction) | `apply_event` con match de scancodes | Extraer `KeyAction` enum y tabla de keybindings |
| `workspaces.rs` | Array plano de ventanas | Añadir `Workspace` (grupos de ventanas) |
| `shell/` (decoraciones) | `SimpleWindow` + draw_windows | Mantener, mejorar estilo |
| `cursor.rs` | `draw_cursor` en InputState | Ya existe; mejorar si hace falta |
| `config/` | Constantes SCANCODE_* | Crear `Keybindings` configurables |

---

## 3. Patrones portables a implementar

### 3.1 Focus explícito

**xfwl4**: `KeyboardFocusTarget`, `PointerFocusTarget` — objeto que recibe input.

**smithay_app** (adaptado):
```rust
// Índice de ventana con focus (-1 = ninguna/escritorio)
focused_window: Option<usize>,

fn focus_window(&mut self, idx: Option<usize>) { ... }
fn focus_under_cursor(&mut self, cursor_x: i32, cursor_y: i32, windows: &[SimpleWindow], count: usize) -> Option<usize>
```

### 3.2 Ciclado de ventanas (Alt+Tab)

**xfwl4**: `collect_tabwin_clients`, `cycle_workspaces`, orden z.

**smithay_app** (adaptado):
```rust
fn cycle_focus_forward(&mut self, windows: &[SimpleWindow], count: usize) -> Option<usize>
fn cycle_focus_backward(&mut self, windows: &[SimpleWindow], count: usize) -> Option<usize>
// Ciclar: focused → siguiente ventana en z-order
```

### 3.3 Keybindings como enum de acciones

**xfwl4**: `KeyAction` (Quit, Run, TogglePreview, CycleWindow, …).

**smithay_app** (adaptado):
```rust
enum KeyAction {
    None,
    Clear,
    NewWindow,
    CloseWindow,
    CycleForward,
    CycleBackward,
    CenterCursor,
    SetColor(u8),
    // ...
}

fn scancode_to_action(scancode: u16, modifiers: u8) -> KeyAction
```

### 3.4 Workspaces (opcional, fase posterior)

**xfwl4**: `Workspace { id, space, name, position }`, `WorkspaceManager`.

**smithay_app** (adaptado):
```rust
struct Workspace {
    windows: [SimpleWindow; MAX_WINDOWS],
    count: usize,
}
// workspace_active: usize
```

---

## 4. Roadmap sugerido

### Fase 1 — Estructura ✅
- [x] Extraer `CompositorState` (ventanas + focus + configuración)
- [x] Separar `Keybindings` (scancode → acción, enum KeyAction)
- [x] Añadir `focused_window: Option<usize>`

### Fase 2 — Focus y ciclado ✅
- [x] `focus_under_cursor`: clic en ventana da focus
- [x] `cycle_focus_forward` / `cycle_focus_backward` (Tab / `)
- [x] Keybinding: Tab adelante, ` (backtick) atrás (sin Alt; input_service no envía modifiers)

### Fase 3 — Mejoras de ventanas ✅
- [x] Orden z explícito (raise al ciclar con Tab/`)
- [x] Indicador visual de ventana enfocada (borde más brillante)
- [x] Keybinding: Escape cierra ventana enfocada

### Fase 4 — Extras (opcional)
- [ ] Workspaces básicos (2–4 espacios)
- [x] Minimizar ventana (M minimiza, R restaura última; cycle omite minimizadas)
- [ ] Configuración de keybindings en tiempo de compilación

---

## 5. Referencias de código en xfwl4

| Concepto | Archivo xfwl4 |
|----------|----------------|
| KeyAction, process_common_key_action | `input_handler.rs` |
| Focus (KeyboardFocusTarget) | `focus.rs` |
| Ciclado de ventanas | `cycle.rs` (collect_tabwin_clients) |
| Workspaces | `workspaces.rs` |
| Decoraciones SSD | `shell/ssd.rs`, `shell/element.rs` |

---

## 6. Limitaciones de Eclipse

- **no_std**: sin `std::`, sin `Command::spawn`, sin `HashMap` (usar arrays)
- **Input**: scancodes PS/2 vía IPC, no xkbcommon
- **Display**: framebuffer directo, no Wayland/DRM
- **Sin glib/gtk**: UI propia con `embedded_graphics`

Los patrones de **lógica** (focus, ciclado, keybindings) son portables; las APIs concretas (Wayland, udev, etc.) se sustituyen por las de Eclipse.
