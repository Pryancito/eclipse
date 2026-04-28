//! Vista (toplevel/view) — equivalente a `view.c` en labwc.
//!
//! En labwc cada ventana visible se llama "view" y puede tener distintos tipos
//! (xdg, layer, xwayland). Aquí lo replicamos con `View::content`.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    /// xdg_toplevel (cliente Wayland normal con xdg-shell).
    XdgToplevel,
    /// zwlr_layer_surface_v1 (panels, docks, fondos…).
    LayerShell,
    /// XWayland surface (cliente X11 nested).
    Xwayland,
}

#[derive(Clone, Debug)]
pub struct View {
    pub kind:    ViewKind,
    /// Identidad: PID del cliente + ObjectId de la wl_surface.
    pub client_pid:  u32,
    pub surface_id:  u32,
    /// Geometría actual (en píxeles del output).
    pub x: i32, pub y: i32, pub w: i32, pub h: i32,
    /// Geometría guardada antes de maximizar/fullscreen.
    pub stored: (i32, i32, i32, i32),
    pub maximized: bool,
    pub minimized: bool,
    pub fullscreen: bool,
    /// `true` si el cliente pidió SSD (zxdg_decoration_manager_v1).
    pub ssd: bool,
    /// `true` si el view está siendo cerrado (esperando ack del cliente).
    pub closing: bool,
    pub workspace: u32,
    pub title: String,
    pub app_id: String,
    /// z-order (mayor = más arriba). El stack se reordena al focus/raise.
    pub z: u32,
}

impl View {
    pub fn new(kind: ViewKind, pid: u32, surf: u32, w: i32, h: i32) -> Self {
        Self {
            kind, client_pid: pid, surface_id: surf,
            x: 100, y: 100, w, h,
            stored: (100, 100, w, h),
            maximized: false, minimized: false, fullscreen: false,
            ssd: true, closing: false, workspace: 1,
            title: String::new(), app_id: String::new(),
            z: 0,
        }
    }

    pub fn rect(&self) -> (i32, i32, i32, i32) { (self.x, self.y, self.w, self.h) }

    pub fn maximize(&mut self, screen_w: i32, screen_h: i32, title_h: i32) {
        if self.maximized { return; }
        self.stored = (self.x, self.y, self.w, self.h);
        self.x = 0; self.y = title_h;
        self.w = screen_w;
        self.h = screen_h - title_h;
        self.maximized = true;
    }

    pub fn unmaximize(&mut self) {
        if !self.maximized { return; }
        let (sx, sy, sw, sh) = self.stored;
        self.x = sx; self.y = sy; self.w = sw; self.h = sh;
        self.maximized = false;
    }

    pub fn toggle_maximize(&mut self, w: i32, h: i32, title_h: i32) {
        if self.maximized { self.unmaximize(); } else { self.maximize(w, h, title_h); }
    }
}

/// Stacking order: vector de índices ordenado por `z` descendente.
#[derive(Default)]
pub struct Stack {
    pub views: Vec<View>,
    pub focused: Option<usize>,
    next_z: u32,
}

impl Stack {
    pub fn map(&mut self, mut v: View) -> usize {
        self.next_z += 1;
        v.z = self.next_z;
        self.views.push(v);
        let idx = self.views.len() - 1;
        self.focused = Some(idx);
        idx
    }

    pub fn unmap(&mut self, idx: usize) {
        if idx < self.views.len() { self.views[idx].closing = true; }
        if self.focused == Some(idx) { self.focus_next(); }
    }

    /// Sube el view al tope del stack (raise).
    pub fn raise(&mut self, idx: usize) {
        if idx >= self.views.len() { return; }
        self.next_z += 1;
        self.views[idx].z = self.next_z;
        self.focused = Some(idx);
    }

    /// Alt+Tab: foco al siguiente view no-cerrado, no-minimizado.
    pub fn focus_next(&mut self) {
        let n = self.views.len();
        if n == 0 { self.focused = None; return; }
        let start = self.focused.map(|i| (i + 1) % n).unwrap_or(0);
        for off in 0..n {
            let i = (start + off) % n;
            if !self.views[i].closing && !self.views[i].minimized {
                self.focused = Some(i);
                self.raise(i);
                return;
            }
        }
        self.focused = None;
    }

    pub fn focus_prev(&mut self) {
        let n = self.views.len();
        if n == 0 { self.focused = None; return; }
        let start = self.focused.map(|i| (i + n - 1) % n).unwrap_or(0);
        for off in 0..n {
            let i = (start + n - off) % n;
            if !self.views[i].closing && !self.views[i].minimized {
                self.focused = Some(i);
                self.raise(i);
                return;
            }
        }
    }

    /// Hit-test por z descendente. Devuelve índice del primer view bajo (cx,cy).
    pub fn hit(&self, cx: i32, cy: i32) -> Option<usize> {
        let mut order: Vec<usize> = (0..self.views.len()).collect();
        order.sort_by(|a, b| self.views[*b].z.cmp(&self.views[*a].z));
        for i in order {
            let v = &self.views[i];
            if v.closing || v.minimized { continue; }
            if cx >= v.x && cy >= v.y && cx < v.x + v.w && cy < v.y + v.h {
                return Some(i);
            }
        }
        None
    }

    /// Devuelve los índices ordenados de fondo a frente (para pintar).
    pub fn paint_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.views.len()).collect();
        order.sort_by(|a, b| self.views[*a].z.cmp(&self.views[*b].z));
        order
    }
}
