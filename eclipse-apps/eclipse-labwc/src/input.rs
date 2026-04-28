//! Procesamiento de eventos de entrada (cursor, teclado, drag).
//!
//! Mantiene estado de "qué está haciendo el ratón ahora mismo" (idle / dragging-move /
//! dragging-resize / drawing-menu) — análogo a `interactive.c` de labwc.

use crate::actions::Action;
use crate::config::{LabwcConfig, MouseContext, MouseEvent, MouseButton};
use crate::key::{Mods, evdev_to_xkb, match_keybind};
use crate::menu::MenuOverlay;
use crate::ssd::{Hit, BorderEdge, TitleButton, hit_test};
use crate::theme::Theme;
use crate::view::Stack;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractiveMode {
    Idle,
    Move    { view: usize, dx: i32, dy: i32 },
    Resize  { view: usize, edge: BorderEdge, anchor_x: i32, anchor_y: i32, anchor_w: i32, anchor_h: i32 },
    MenuOpen,
}

pub struct InputState {
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub mods: Mods,
    pub mode: InteractiveMode,
    pub menu: Option<MenuOverlay>,
    pub screen_w: i32,
    pub screen_h: i32,
}

impl InputState {
    pub fn new(w: i32, h: i32) -> Self {
        Self {
            cursor_x: w / 2, cursor_y: h / 2,
            mods: Mods::empty(),
            mode: InteractiveMode::Idle,
            menu: None,
            screen_w: w, screen_h: h,
        }
    }

    /// Procesa un evdev `EV_KEY` (teclado). Devuelve la action a ejecutar (o None).
    pub fn handle_key(&mut self, code: u32, pressed: bool, cfg: &LabwcConfig) -> Option<Action> {
        // Track modifiers
        match code {
            29 | 97   => { self.mods.set(Mods::CTRL,  pressed); return None; }
            42 | 54   => { self.mods.set(Mods::SHIFT, pressed); return None; }
            56 | 100  => { self.mods.set(Mods::ALT,   pressed); return None; }
            125 | 126 => { self.mods.set(Mods::SUPER, pressed); return None; }
            _ => {}
        }
        if !pressed { return None; }
        let key = evdev_to_xkb(code)?;
        match_keybind(&cfg.keybinds, self.mods, key).cloned()
    }

    /// Procesa motion del ratón.
    pub fn handle_motion(&mut self, dx: i32, dy: i32, stack: &mut Stack) {
        self.cursor_x = (self.cursor_x + dx).clamp(0, self.screen_w - 1);
        self.cursor_y = (self.cursor_y + dy).clamp(0, self.screen_h - 1);

        if let InteractiveMode::Move { view, dx: ox, dy: oy } = self.mode {
            if view < stack.views.len() {
                stack.views[view].x = self.cursor_x - ox;
                stack.views[view].y = self.cursor_y - oy;
            }
        }
        if let InteractiveMode::Resize { view, edge, anchor_x, anchor_y, anchor_w, anchor_h } = self.mode {
            if view < stack.views.len() {
                let v = &mut stack.views[view];
                let dx = self.cursor_x - anchor_x;
                let dy = self.cursor_y - anchor_y;
                match edge {
                    BorderEdge::E   => { v.w = (anchor_w + dx).max(80); }
                    BorderEdge::W   => { v.x = anchor_x + dx; v.w = (anchor_w - dx).max(80); }
                    BorderEdge::S   => { v.h = (anchor_h + dy).max(60); }
                    BorderEdge::N   => { v.y = anchor_y + dy; v.h = (anchor_h - dy).max(60); }
                    BorderEdge::SE  => { v.w = (anchor_w + dx).max(80); v.h = (anchor_h + dy).max(60); }
                    BorderEdge::SW  => { v.x = anchor_x + dx; v.w = (anchor_w - dx).max(80); v.h = (anchor_h + dy).max(60); }
                    BorderEdge::NE  => { v.y = anchor_y + dy; v.w = (anchor_w + dx).max(80); v.h = (anchor_h - dy).max(60); }
                    BorderEdge::NW  => { v.x = anchor_x + dx; v.y = anchor_y + dy; v.w = (anchor_w - dx).max(80); v.h = (anchor_h - dy).max(60); }
                }
            }
        }
    }

    /// Procesa botón del ratón. Devuelve la action a ejecutar (si algún
    /// mousebind del rc.xml coincide o un click sobre un botón del SSD).
    pub fn handle_button(
        &mut self,
        button: MouseButton,
        pressed: bool,
        stack: &mut Stack,
        theme: &Theme,
        cfg: &LabwcConfig,
    ) -> Option<Action> {
        let cx = self.cursor_x; let cy = self.cursor_y;

        // Release: terminar drag.
        if !pressed {
            self.mode = match self.mode {
                InteractiveMode::Move { .. } | InteractiveMode::Resize { .. } => InteractiveMode::Idle,
                m => m,
            };
            return None;
        }

        // ¿Hay menú abierto?
        if let Some(menu) = self.menu.take() {
            if let Some(idx) = menu.hit(cx, cy) {
                let item = &menu.menu.items[idx];
                if let crate::menu::MenuKind::Action(a) = &item.kind {
                    return Some(a.clone());
                }
            }
            // Click fuera del menú → cerrarlo.
            self.mode = InteractiveMode::Idle;
            return None;
        }

        // ¿El clic cayó sobre un view?
        if let Some(idx) = stack.hit(cx, cy) {
            let v = &stack.views[idx];
            let h = hit_test(v, theme, cx, cy);
            stack.raise(idx);

            // Botones del SSD
            if let Hit::Button(btn) = h {
                return Some(match btn {
                    TitleButton::Close    => Action::Close,
                    TitleButton::Maximize => Action::ToggleMaximize,
                    TitleButton::Minimize => Action::Iconify,
                });
            }

            // Drag de move sobre la titlebar
            if let (Hit::Title, MouseButton::Left) = (h, button) {
                self.mode = InteractiveMode::Move {
                    view: idx, dx: cx - v.x, dy: cy - v.y,
                };
                return None;
            }

            // Drag de resize sobre el borde
            if let (Hit::Border(edge), MouseButton::Left) = (h, button) {
                self.mode = InteractiveMode::Resize {
                    view: idx, edge, anchor_x: cx, anchor_y: cy,
                    anchor_w: v.w, anchor_h: v.h,
                };
                return None;
            }
            return None;
        }

        // Click en el root — buscar mousebind con context=Root.
        let want_btn = matches!(button, MouseButton::Right);
        if want_btn {
            for mb in &cfg.mousebinds {
                if mb.context == MouseContext::Root
                    && mb.event == MouseEvent::Press
                    && mb.button == button
                {
                    return Some(mb.action.clone());
                }
            }
        }
        None
    }
}
