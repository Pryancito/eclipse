//! Acciones de labwc — equivalente a `actions.c` upstream.
//!
//! Cada `Action` se ejecuta sobre `LabwcState`. Mantenemos los nombres tal cual
//! los conoce labwc para que `rc.xml` existentes funcionen sin modificaciones.

use alloc::string::String;

#[derive(Clone, Debug)]
pub enum Action {
    /// Cerrar ventana enfocada (envía `xdg_toplevel.close`).
    Close,
    /// Minimizar.
    Iconify,
    /// Maximizar / restaurar.
    ToggleMaximize,
    Maximize,
    /// Iniciar drag de movimiento (interactive move).
    Move,
    /// Iniciar drag de resize.
    Resize,
    /// Ciclar al siguiente/anterior toplevel (Alt+Tab).
    NextWindow,
    PreviousWindow,
    /// Recarga `rc.xml` y `themerc`.
    Reconfigure,
    /// Salir del compositor.
    Exit,
    /// Mostrar menú raíz por nombre (`menu.xml`).
    ShowMenu(String),
    /// Lanzar comando como subproceso (autostart, mousebind, keybind).
    Execute(String),
    /// Ir a workspace n (1..=N).
    GoToDesktop(u32),
    /// No-op explícito (placeholder en pruebas).
    Nop,
}
