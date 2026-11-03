//! Ratón: clics y arrastres en el cromo y la página, y la forma del cursor.

use super::*;

impl App {
    pub(super) fn on_pointer_button(&mut self, button: u32, down: bool) {
        if !down {
            self.loc_drag = false;
            if let Some(from) = self.drag_tab.take() {
                let x = self.px.round() as i32;
                let y = self.py.round() as i32;
                self.finish_tab_drag(from, x, y);
            }
            if self.page_button == Some(button) {
                self.page_button = None;
                if let Some((px, py)) = self.page_px() {
                    self.send_button(button, px, py, false);
                }
                if button == 0x110 {
                    // Fin de un clic o arrastre: lo seleccionado va a la
                    // selección primaria.
                    if let Some(session) = &self.session {
                        session.shell.page_selection_to_primary();
                    }
                }
            }
            self.sync_primary();
            return;
        }
        let x = self.px.round() as i32;
        let y = self.py.round() as i32;
        // Clic en menú contextual (overlay del cromo a pantalla completa).
        if let Some(popup) = self.session.as_ref().and_then(|s| s.shell.context_popup()) {
            if let Some(index) = popup.hit(x, y) {
                if let Some(session) = &self.session {
                    session.shell.activate_context_menu(index);
                }
                self.need_paint = true;
                return;
            }
            if let Some(session) = &self.session {
                session.shell.dismiss_context_menu();
            }
            self.need_paint = true;
            if y >= self.chrome_h() {
                // El clic fuera cierra el menú; no reenvía al page en este down.
                return;
            }
        }
        if y < self.chrome_h() {
            if button == 0x112 {
                self.paste_primary_in_location(x, y);
                return;
            }
            self.on_chrome_click(x, y);
            return;
        }
        let Some((px, py)) = self.page_px() else {
            return;
        };
        if button == 0x113 {
            if let Some(session) = &self.session {
                session.shell.go_back();
            }
            return;
        }
        if button == 0x114 {
            if let Some(session) = &self.session {
                session.shell.go_forward();
            }
            return;
        }
        self.loc_focused = false;
        if let Some(session) = &self.session {
            session.shell.set_page_focused(true);
        }
        self.page_button = Some(button);
        self.send_button(button, px, py, true);
        if button == 0x112 {
            let text = clipboard::get_primary();
            if !text.is_empty() {
                if let Some(session) = &self.session {
                    session.shell.paste_primary_at(px, py, &text);
                }
            }
        }
        self.need_paint = true;
    }

    pub(super) fn send_button(&self, button: u32, px: f32, py: f32, down: bool) {
        let mouse = match button {
            0x110 => MouseButton::Primary,
            0x111 => MouseButton::Secondary,
            0x112 => MouseButton::Auxiliary,
            other => MouseButton::Other(other as u16),
        };
        if let Some(session) = &self.session {
            session.shell.pointer(px, py, Some((mouse, down)));
        }
    }

    pub(super) fn on_chrome_click(&mut self, x: i32, y: i32) {
        let tabs = self
            .session
            .as_ref()
            .map(|s| s.shell.tab_titles().len())
            .unwrap_or(1)
            .max(1);
        let Some(hit) = chrome::hit(self.logical_w, tabs, x, y) else {
            return;
        };
        let was_focused = self.loc_focused;
        self.loc_focused = matches!(hit, Hit::Location);
        if let Some(session) = &self.session {
            session.shell.set_page_focused(!self.loc_focused);
        }
        match hit {
            Hit::Location => {
                if !was_focused {
                    // Primer clic: toda la dirección seleccionada, como en
                    // Firefox, lista para escribir otra encima.
                    if let Some(session) = &self.session {
                        self.loc.set(session.shell.location_text());
                    }
                    self.loc.select_all();
                    self.loc_clicked = None;
                } else {
                    let index = self.location_index_at(x);
                    let double = self.loc_clicked.is_some_and(|(at, cx)| {
                        at.elapsed() < Duration::from_millis(400) && (cx - x).abs() < 4
                    });
                    if double {
                        self.loc.select_word_at(index);
                        self.loc_clicked = None;
                    } else {
                        self.loc.move_to(index, self.shift);
                        self.loc_drag = true;
                        self.loc_clicked = Some((Instant::now(), x));
                    }
                }
            }
            Hit::Back => {
                if let Some(session) = &self.session {
                    session.shell.go_back();
                }
            }
            Hit::Forward => {
                if let Some(session) = &self.session {
                    session.shell.go_forward();
                }
            }
            Hit::Reload => {
                if let Some(session) = &self.session {
                    session.shell.reload();
                }
            }
            Hit::NewTab => {
                if let Some(session) = &self.session {
                    session.shell.new_tab(home::home_url());
                }
            }
            Hit::Tab(index) => {
                if let Some(session) = &self.session {
                    session.shell.select_tab(index);
                }
                // Inicio de arrastre para reordenar al soltar.
                self.drag_tab = Some(index);
            }
            Hit::Close(index) => {
                let keep = self
                    .session
                    .as_ref()
                    .is_none_or(|session| session.shell.close_tab(index));
                if !keep {
                    self.exit = true;
                }
            }
        }
        self.need_paint = true;
    }

    pub(super) fn finish_tab_drag(&mut self, from: usize, x: i32, y: i32) {
        if y < 0 || y >= chrome::HEIGHT {
            return;
        }
        let tabs = self
            .session
            .as_ref()
            .map(|s| s.shell.tab_titles().len())
            .unwrap_or(1)
            .max(1);
        let Some(hit) = chrome::hit(self.logical_w, tabs, x, y) else {
            return;
        };
        let to = match hit {
            Hit::Tab(i) | Hit::Close(i) => i,
            _ => return,
        };
        if to == from {
            return;
        }
        if let Some(session) = &self.session {
            session.shell.move_tab(from, to);
        }
        self.need_paint = true;
    }

    pub(super) fn page_px(&self) -> Option<(f32, f32)> {
        let chrome_h = self.chrome_h() as f64;
        if self.py < chrome_h {
            return None;
        }
        let scale = self.scale as f64;
        Some((
            (self.px * scale) as f32,
            ((self.py - chrome_h) * scale) as f32,
        ))
    }

    pub(super) fn note_motion(&mut self, x: f64, y: f64) {
        let was = self.on_page;
        self.px = x;
        self.py = y;
        self.on_page = self.py >= self.chrome_h() as f64;
        if self.loc_drag && self.loc_focused {
            let index = self.location_index_at(x.round() as i32);
            self.loc.move_to(index, true);
            self.need_paint = true;
        }
        if was && !self.on_page {
            if let Some(session) = &self.session {
                session.shell.pointer_left();
            }
        }
        if let Some((px, py)) = self.page_px() {
            if let Some(session) = &self.session {
                session.shell.pointer(px, py, None);
            }
        }
        self.update_cursor();
    }

    /// Cursor que toca bajo el puntero: el que pidió la página, o el del cromo
    /// (mano sobre botones y pestañas, I sobre la barra).
    pub(super) fn wanted_cursor(&self) -> Cursor {
        if self.on_page {
            return self
                .session
                .as_ref()
                .map_or(Cursor::Default, |s| s.shell.page_cursor());
        }
        if self.drag_tab.is_some() {
            return Cursor::Grabbing;
        }
        let tabs = self
            .session
            .as_ref()
            .map(|s| s.shell.tab_titles().len())
            .unwrap_or(1)
            .max(1);
        chrome_cursor(chrome::hit(
            self.logical_w,
            tabs,
            self.px as i32,
            self.py as i32,
        ))
    }

    pub(super) fn update_cursor(&mut self) {
        let Some(serial) = self.pointer_serial else {
            return;
        };
        let wanted = self.wanted_cursor();
        if self.cursor_applied == Some(wanted) {
            return;
        }
        if self.cursor_dev.is_none() {
            if let (Some(mgr), Some(pointer)) = (&self.cursor_mgr, &self.pointer) {
                self.cursor_dev = Some(mgr.get_pointer(pointer, &self.qh, ()));
            }
        }
        if wanted == Cursor::None {
            if let Some(pointer) = &self.pointer {
                pointer.set_cursor(serial, None, 0, 0);
            }
        } else if let Some(dev) = &self.cursor_dev {
            dev.set_shape(serial, cursor_shape(wanted));
        } else {
            return;
        }
        self.cursor_applied = Some(wanted);
    }
}

/// Cursor del cromo según lo que hay bajo el puntero.
pub(super) fn chrome_cursor(hit: Option<Hit>) -> Cursor {
    match hit {
        Some(Hit::Location) => Cursor::Text,
        Some(_) => Cursor::Pointer,
        None => Cursor::Default,
    }
}

/// `servo::Cursor` → forma de `wp_cursor_shape_v1` (mismos nombres CSS).
pub(super) fn cursor_shape(cursor: Cursor) -> Shape {
    match cursor {
        Cursor::None | Cursor::Default => Shape::Default,
        Cursor::Pointer => Shape::Pointer,
        Cursor::ContextMenu => Shape::ContextMenu,
        Cursor::Help => Shape::Help,
        Cursor::Progress => Shape::Progress,
        Cursor::Wait => Shape::Wait,
        Cursor::Cell => Shape::Cell,
        Cursor::Crosshair => Shape::Crosshair,
        Cursor::Text => Shape::Text,
        Cursor::VerticalText => Shape::VerticalText,
        Cursor::Alias => Shape::Alias,
        Cursor::Copy => Shape::Copy,
        Cursor::Move => Shape::Move,
        Cursor::NoDrop => Shape::NoDrop,
        Cursor::NotAllowed => Shape::NotAllowed,
        Cursor::Grab => Shape::Grab,
        Cursor::Grabbing => Shape::Grabbing,
        Cursor::EResize => Shape::EResize,
        Cursor::NResize => Shape::NResize,
        Cursor::NeResize => Shape::NeResize,
        Cursor::NwResize => Shape::NwResize,
        Cursor::SResize => Shape::SResize,
        Cursor::SeResize => Shape::SeResize,
        Cursor::SwResize => Shape::SwResize,
        Cursor::WResize => Shape::WResize,
        Cursor::EwResize => Shape::EwResize,
        Cursor::NsResize => Shape::NsResize,
        Cursor::NeswResize => Shape::NeswResize,
        Cursor::NwseResize => Shape::NwseResize,
        Cursor::ColResize => Shape::ColResize,
        Cursor::RowResize => Shape::RowResize,
        Cursor::AllScroll => Shape::AllScroll,
        Cursor::ZoomIn => Shape::ZoomIn,
        Cursor::ZoomOut => Shape::ZoomOut,
    }
}
