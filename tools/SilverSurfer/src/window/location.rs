//! Barra de dirección y de búsqueda: edición, selección primaria, abrir fichero.

use super::*;

impl App {
    pub(super) fn leave_location(&mut self) {
        if !self.loc_focused && !self.find_mode {
            return;
        }
        self.loc_focused = false;
        self.find_mode = false;
        if let Some(session) = &self.session {
            session.shell.set_page_focused(true);
        }
        self.need_paint = true;
    }

    pub(super) fn open_file(&mut self) {
        let Ok(paths) = files::pick(&PickRequest {
            multiple: false,
            save_name: None,
        }) else {
            return;
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        if let Ok(url) = url::Url::from_file_path(&path) {
            if let Some(session) = &self.session {
                session.shell.load(url);
            }
            self.loc_focused = false;
            self.need_paint = true;
        }
    }

    pub(super) fn edit_location(&mut self, evdev: u32) {
        let (shift, word) = (self.shift, self.ctrl);
        match evdev {
            1 => {
                self.leave_location();
            }
            15 if !self.find_mode => {
                // Tab — aceptar autocompletado del historial.
                if let Some(visit) = history::load().autocomplete(self.loc.text()) {
                    self.loc.set(visit.url.clone());
                }
            }
            28 | 96 => {
                let text = self.loc.text().to_owned();
                if self.find_mode {
                    if let Some(session) = &self.session {
                        session.shell.find_in_page(&text);
                    }
                } else if let Ok(url) = resolve_omnibox(&text, &search_template()) {
                    if let Some(session) = &self.session {
                        session.shell.load(url);
                    }
                    self.leave_location();
                } else {
                    self.leave_location();
                }
            }
            14 => self.loc.backspace(word),
            111 => self.loc.delete(word),
            105 => self.loc.left(shift, word),
            106 => self.loc.right(shift, word),
            102 => self.loc.home(shift),
            107 => self.loc.end(shift),
            // Ctrl+A: seleccionar todo.
            30 if self.ctrl => self.loc.select_all(),
            // Ctrl+C / Ctrl+X: la selección, o la dirección entera si no hay.
            46 | 45 if self.ctrl => {
                if self.loc.selection().is_none() {
                    self.loc.select_all();
                }
                if let Some(text) = self.loc.selected_text() {
                    clipboard::set_text(&text);
                }
                if evdev == 45 {
                    self.loc.delete_selection();
                }
            }
            47 if self.ctrl => {
                let text = paste_line(&clipboard::get_text());
                self.loc.insert(&text);
            }
            _ => {
                if let Some(text) = self.key_text(evdev) {
                    self.loc.insert(&text);
                }
            }
        }
    }

    /// Lo seleccionado en la barra pasa a la selección primaria.
    pub(super) fn sync_primary(&mut self) {
        if !self.loc_focused || self.find_mode {
            return;
        }
        if let Some(text) = self.loc.selected_text() {
            clipboard::set_primary(&text);
        }
    }

    /// Botón central en la barra: enfoca (sin seleccionar) y pega la
    /// selección primaria donde cae el clic.
    pub(super) fn paste_primary_in_location(&mut self, x: i32, y: i32) {
        let tabs = self
            .session
            .as_ref()
            .map(|s| s.shell.tab_titles().len())
            .unwrap_or(1)
            .max(1);
        if chrome::hit(self.logical_w, tabs, x, y) != Some(Hit::Location) {
            return;
        }
        let text = paste_line(&clipboard::get_primary());
        if !self.loc_focused {
            self.loc_focused = true;
            self.find_mode = false;
            if let Some(session) = &self.session {
                session.shell.set_page_focused(false);
                self.loc.set(session.shell.location_text());
            }
        }
        let index = self.location_index_at(x);
        self.loc.move_to(index, false);
        self.loc.insert(&text);
        self.need_paint = true;
    }

    /// Índice de carácter bajo la x (px CSS de la ventana) en la barra.
    pub(super) fn location_index_at(&self, x: i32) -> usize {
        let tabs = self
            .session
            .as_ref()
            .map(|s| s.shell.tab_titles().len())
            .unwrap_or(1)
            .max(1);
        let private = self.session.as_ref().is_some_and(|s| s.private);
        let lay = chrome::layout_ex(self.logical_w, tabs, private);
        let room = (lay.location.w / 6).max(1) as usize;
        let column = ((x - lay.location.x - 8).max(0) + 3) / 6;
        let len = self.loc.text().chars().count();
        lineedit::column_to_index(len, room, self.loc.cursor(), column as usize)
    }
}
