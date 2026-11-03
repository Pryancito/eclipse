//! Portapapeles del sistema para la página y la barra de dirección.
//!
//! `arboard` habla wlr/ext data-control en Wayland (labwc expone ambos), así
//! que leer y escribir no necesita el serial del foco de teclado. Si el
//! compositor no tiene data-control, arboard prueba X11; si tampoco, queda un
//! texto en memoria que al menos sirve entre pestañas de esta ventana.

use std::sync::{Mutex, OnceLock};

use servo::{ClipboardDelegate, StringRequest, WebView};

static SYSTEM: OnceLock<Option<Mutex<arboard::Clipboard>>> = OnceLock::new();
static LOCAL: Mutex<String> = Mutex::new(String::new());
static LOCAL_PRIMARY: Mutex<String> = Mutex::new(String::new());

fn with_system<T>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Option<T> {
    let slot = SYSTEM.get_or_init(|| match arboard::Clipboard::new() {
        Ok(cb) => Some(Mutex::new(cb)),
        Err(err) => {
            eprintln!("silversurfer: portapapeles del sistema no disponible: {err}");
            None
        }
    });
    let mut cb = slot.as_ref()?.lock().ok()?;
    f(&mut cb).ok()
}

/// Texto del portapapeles: el del sistema si lo hay, si no el local.
pub fn get_text() -> String {
    if let Some(text) = with_system(|cb| cb.get_text()) {
        return text;
    }
    LOCAL.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_text(text: &str) {
    if let Ok(mut local) = LOCAL.lock() {
        local.clear();
        local.push_str(text);
    }
    with_system(|cb| cb.set_text(text));
}

/// Selección primaria (seleccionar y pegar con el botón central).
pub fn get_primary() -> String {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    if let Some(text) = with_system(|cb| cb.get().clipboard(LinuxClipboardKind::Primary).text()) {
        return text;
    }
    LOCAL_PRIMARY.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_primary(text: &str) {
    use arboard::{LinuxClipboardKind, SetExtLinux};
    if let Ok(mut local) = LOCAL_PRIMARY.lock() {
        if *local == text {
            return;
        }
        local.clear();
        local.push_str(text);
    }
    with_system(|cb| cb.set().clipboard(LinuxClipboardKind::Primary).text(text));
}

pub fn clear() {
    if let Ok(mut local) = LOCAL.lock() {
        local.clear();
    }
    with_system(|cb| cb.clear());
}

/// Delegado de Servo: `navigator.clipboard`, copiar/pegar en campos de texto.
pub struct Delegate;

impl ClipboardDelegate for Delegate {
    fn clear(&self, _webview: WebView) {
        clear();
    }

    fn get_text(&self, _webview: WebView, request: StringRequest) {
        request.success(get_text());
    }

    fn set_text(&self, _webview: WebView, new_contents: String) {
        set_text(&new_contents);
    }
}
