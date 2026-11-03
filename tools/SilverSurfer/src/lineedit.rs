//! Edición de una línea (barra de dirección): cursor, selección y palabras.
//!
//! Las posiciones son en caracteres, no en bytes. La selección va del ancla
//! al cursor; sin ancla (o con ancla igual al cursor) no hay selección.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

impl LineEdit {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Sustituye el texto, con el cursor al final y sin selección.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.len();
        self.anchor = None;
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    /// Rango seleccionado `[inicio, fin)` en caracteres, si no está vacío.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
        (a < b).then_some((a, b))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection()?;
        Some(self.text.chars().skip(a).take(b - a).collect())
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.len();
    }

    /// Borra la selección; `true` si había algo seleccionado.
    pub fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.text = self
            .text
            .chars()
            .enumerate()
            .filter(|(i, _)| *i < a || *i >= b)
            .map(|(_, ch)| ch)
            .collect();
        self.cursor = a;
        self.anchor = None;
        true
    }

    /// Escribe `text` en el cursor, sustituyendo la selección.
    pub fn insert(&mut self, text: &str) {
        self.delete_selection();
        let at = self
            .text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(i, _)| i);
        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// Retroceso: borra la selección o el carácter (la palabra con `word`)
    /// anterior al cursor.
    pub fn backspace(&mut self, word: bool) {
        if self.delete_selection() || self.cursor == 0 {
            return;
        }
        let to = if word {
            self.word_left()
        } else {
            self.cursor - 1
        };
        self.anchor = Some(to);
        self.delete_selection();
    }

    /// Supr: borra la selección o el carácter (la palabra) tras el cursor.
    pub fn delete(&mut self, word: bool) {
        if self.delete_selection() || self.cursor >= self.len() {
            return;
        }
        let to = if word {
            self.word_right()
        } else {
            self.cursor + 1
        };
        self.anchor = Some(to);
        self.delete_selection();
    }

    /// Lleva el cursor a `pos`. Con `extend` (Shift) amplía la selección; sin
    /// él la quita.
    pub fn move_to(&mut self, pos: usize, extend: bool) {
        let pos = pos.min(self.len());
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
    }

    /// Flecha izquierda/derecha. Sin Shift y con selección, colapsa al borde
    /// de la selección como hacen los navegadores.
    pub fn left(&mut self, extend: bool, word: bool) {
        if !extend {
            if let Some((a, _)) = self.selection() {
                self.move_to(a, false);
                return;
            }
        }
        let to = if word {
            self.word_left()
        } else {
            self.cursor.saturating_sub(1)
        };
        self.move_to(to, extend);
    }

    pub fn right(&mut self, extend: bool, word: bool) {
        if !extend {
            if let Some((_, b)) = self.selection() {
                self.move_to(b, false);
                return;
            }
        }
        let to = if word {
            self.word_right()
        } else {
            self.cursor + 1
        };
        self.move_to(to, extend);
    }

    pub fn home(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub fn end(&mut self, extend: bool) {
        let n = self.len();
        self.move_to(n, extend);
    }

    /// Doble clic: selecciona la palabra bajo `pos`.
    pub fn select_word_at(&mut self, pos: usize) {
        let chars: Vec<char> = self.text.chars().collect();
        let pos = pos.min(chars.len());
        let mut a = pos;
        while a > 0 && is_word(chars[a - 1]) {
            a -= 1;
        }
        let mut b = pos;
        while b < chars.len() && is_word(chars[b]) {
            b += 1;
        }
        self.anchor = Some(a);
        self.cursor = b;
    }

    /// Inicio de la palabra a la izquierda del cursor (Ctrl+←).
    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && !is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(chars[i - 1]) {
            i -= 1;
        }
        i
    }

    /// Fin de la palabra a la derecha del cursor (Ctrl+→).
    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i < chars.len() && !is_word(chars[i]) {
            i += 1;
        }
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        i
    }
}

/// Las URL se recorren por trozos: `/`, `.`, `:`, `?`, `&`, `=`… separan.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '-'
}

/// Primer carácter visible de la barra y columna de cada carácter, para el
/// cursor `|` que ocupa una columna. Igual que el recorte de
/// `chrome::location_line`: la ventana de `room - 1` caracteres acaba en el
/// cursor.
pub fn visible_start(len: usize, room: usize, cursor: usize) -> usize {
    let body = room.max(2) - 1;
    cursor.min(len).saturating_sub(body)
}

/// Columna en pantalla (0 = primer carácter visible) → índice de carácter.
/// Las columnas tras el cursor llevan uno de más por el `|`.
pub fn column_to_index(len: usize, room: usize, cursor: usize, column: usize) -> usize {
    let start = visible_start(len, room, cursor);
    let cursor = cursor.min(len);
    let index = start + column;
    let index = if index > cursor { index - 1 } else { index };
    index.min(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str) -> LineEdit {
        let mut e = LineEdit::default();
        e.set(text);
        e
    }

    #[test]
    fn select_all_then_type_replaces() {
        let mut e = edit("https://example.com/");
        e.select_all();
        assert_eq!(e.selected_text().as_deref(), Some("https://example.com/"));
        e.insert("duck");
        assert_eq!(e.text(), "duck");
        assert_eq!(e.cursor(), 4);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn shift_arrows_extend_and_plain_arrow_collapses() {
        let mut e = edit("abcdef");
        e.left(true, false);
        e.left(true, false);
        assert_eq!(e.selected_text().as_deref(), Some("ef"));
        e.left(false, false);
        assert_eq!((e.cursor(), e.selection()), (4, None));
    }

    #[test]
    fn ctrl_arrows_jump_url_pieces() {
        let mut e = edit("https://es.wikipedia.org/wiki/Eñe");
        e.left(false, true);
        assert_eq!(e.cursor(), "https://es.wikipedia.org/wiki/".chars().count());
        e.left(false, true);
        assert_eq!(e.cursor(), "https://es.wikipedia.org/".chars().count());
        e.home(false);
        e.right(false, true);
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn backspace_and_delete_with_and_without_selection() {
        let mut e = edit("hola mundo");
        e.backspace(false);
        assert_eq!(e.text(), "hola mund");
        e.backspace(true);
        assert_eq!(e.text(), "hola ");
        e.home(false);
        e.delete(false);
        assert_eq!(e.text(), "ola ");
        e.select_all();
        e.delete(false);
        assert_eq!(e.text(), "");
    }

    #[test]
    fn double_click_selects_word_with_accents() {
        let mut e = edit("ir a mañana.es");
        e.select_word_at(7);
        assert_eq!(e.selected_text().as_deref(), Some("mañana"));
    }

    #[test]
    fn click_column_maps_around_caret() {
        // "abcdef", room 10, caret at 3 → visible "abc|def"
        assert_eq!(column_to_index(6, 10, 3, 0), 0);
        assert_eq!(column_to_index(6, 10, 3, 3), 3);
        assert_eq!(column_to_index(6, 10, 3, 5), 4);
        assert_eq!(column_to_index(6, 10, 3, 40), 6);
        // Scrolled: 20 chars, room 6 (5 visible), caret at end → start 15.
        assert_eq!(visible_start(20, 6, 20), 15);
        assert_eq!(column_to_index(20, 6, 20, 2), 17);
    }
}
