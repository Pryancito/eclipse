//! What lunarfile can do to the selected entry, and the menu that offers it.
//!
//! It could only look: list, filter, preview, open. Now it acts, the way a
//! file manager does: rename (F2), copy (F5) and cut (F6) then paste in
//! another folder (Insert), make a folder (F7), send to the trash (Supr or
//! F8). Every one of them is also in a menu, opened with the Menu key or a
//! right click on a row, each action in its own colour and with its key
//! beside it. The keys are the function keys and not Ctrl+C because this
//! client reads no modifiers (see `keys`), and they are Midnight Commander's.
//!
//! The work itself is `lunarbar::fileops`, which never replaces a file.
//!
//! A child of `lunarfile.rs` to keep that file under its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;
use lunarbar::fileops;

/// One thing the menu can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Open,
    Rename,
    Copy,
    Cut,
    Paste,
    NewDir,
    Trash,
}

/// The menu, in order.
pub const OPS: [Op; 7] = [Op::Open, Op::Rename, Op::Copy, Op::Cut, Op::Paste, Op::NewDir, Op::Trash];

/// What a name is being typed for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// The new name of this path.
    Rename(String),
    NewDir,
}

pub const MENU_W: i32 = 256;
pub const MENU_ROW_H: i32 = 28;
pub const MENU_HEAD_H: i32 = 28;

/// The key that does `op` without the menu.
pub fn op_key(op: Op) -> &'static str {
    match op {
        Op::Open => "Intro",
        Op::Rename => "F2",
        Op::Copy => "F5",
        Op::Cut => "F6",
        Op::Paste => "Insert",
        Op::NewDir => "F7",
        Op::Trash => "Supr",
    }
}

pub fn op_label(op: Op) -> &'static str {
    match op {
        Op::Open => "Abrir",
        Op::Rename => "Renombrar",
        Op::Copy => "Copiar",
        Op::Cut => "Cortar",
        Op::Paste => "Pegar aquí",
        Op::NewDir => "Nueva carpeta",
        Op::Trash => "A la papelera",
    }
}

/// Each action's own colour, graded by what it does to the file: opening and
/// copying leave it as it is (cyan, green), renaming and making change names
/// (violet, blue), cutting and pasting move it (gold, teal), the trash takes
/// it away (the warning).
pub fn op_colour(op: Op, p: &Palette) -> Rgb {
    match op {
        Op::Open => p.accent,
        Op::Rename => (0xa7, 0x8b, 0xfa),
        Op::Copy => (0x6e, 0xe7, 0xa8),
        Op::Cut => GOLD,
        Op::Paste => (0x4f, 0xb5, 0xa5),
        Op::NewDir => (0x7a, 0xa2, 0xf7),
        Op::Trash => WARN,
    }
}

/// The function keys, and what they do in the browser.
pub fn op_for_key(key: u32) -> Option<Op> {
    Some(match key {
        60 => Op::Rename,      // F2
        63 => Op::Copy,        // F5
        64 => Op::Cut,         // F6
        KEY_INSERT_WL => Op::Paste, // Insert
        65 => Op::NewDir,      // F7
        111 | 66 => Op::Trash, // Delete, F8
        _ => return None,
    })
}

/// The Menu key, and F10 for a keyboard without one.
pub fn is_menu_key(key: u32) -> bool {
    key == 127 || key == 68
}

impl State {
    /// The selected entry's path, not `..`.
    pub(crate) fn chosen(&self) -> Option<String> {
        match self.row(self.sel)? {
            Row::Entry(i) => Some(files::join_child(&self.dir, &self.entries.get(i)?.name)),
            Row::Parent => None,
        }
    }

    /// Whether `op` has something to work on right now.
    pub(crate) fn can(&self, op: Op) -> bool {
        match op {
            Op::Open => self.row(self.sel).is_some(),
            Op::Paste => self.held.is_some(),
            Op::NewDir => true,
            _ => self.chosen().is_some(),
        }
    }

    /// Re-read this folder and put the selection on `name` when it is there.
    fn reload_on(&mut self, name: Option<&str>) {
        let (sel, dir) = (self.sel, self.dir.clone());
        self.chdir(&dir);
        self.sel = sel.min(self.rows.len().saturating_sub(1));
        if let Some(n) = name {
            if let Some(r) = self.rows.iter().position(|r| matches!(r, Row::Entry(i) if self.entries[*i].name == n)) {
                self.sel = r;
            }
        }
        self.move_sel(0, Step::Clamp);
    }

    fn fail(&mut self, what: &str, e: std::io::Error) {
        self.note = format!("No se pudo {what}: {e}");
    }

    /// Do `op` to the selection.
    pub(crate) fn op(&mut self, op: Op) {
        self.menu = None;
        if !self.can(op) {
            self.note = match op {
                Op::Paste => "Nada que pegar: copia o corta algo antes (F5, F6)".into(),
                _ => "Elige un archivo o una carpeta primero".into(),
            };
            return;
        }
        let name_of = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
        match op {
            Op::Open => {
                let sel = self.sel;
                self.activate(sel);
            }
            Op::Rename => {
                let Some(p) = self.chosen() else { return };
                self.kept_filter = std::mem::replace(&mut self.field, name_of(&p));
                self.edit = Some(Edit::Rename(p));
                self.note = "Escribe el nombre nuevo · Intro renombra · Esc cancela".into();
                self.refilter();
            }
            Op::NewDir => {
                let free = fileops::free_name(std::path::Path::new(&self.dir), "Carpeta nueva", true);
                self.kept_filter = std::mem::replace(&mut self.field, free);
                self.edit = Some(Edit::NewDir);
                self.note = "Nombre de la carpeta · Intro la crea · Esc cancela".into();
                self.refilter();
            }
            Op::Copy | Op::Cut => {
                let Some(p) = self.chosen() else { return };
                let cut = op == Op::Cut;
                self.note = format!(
                    "{}: {} · ve a otra carpeta e Insert lo pega",
                    if cut { "Cortado" } else { "Copiado" },
                    name_of(&p)
                );
                self.held = Some((p, cut));
            }
            Op::Paste => {
                let Some((src, cut)) = self.held.clone() else { return };
                let dir = std::path::Path::new(&self.dir).to_path_buf();
                let src_p = std::path::Path::new(&src);
                let r = if cut { fileops::move_into(src_p, &dir) } else { fileops::copy_into(src_p, &dir) };
                match r {
                    Ok(dst) => {
                        let n = dst.file_name().map(|n| n.to_string_lossy().into_owned());
                        if cut {
                            self.held = None;
                        }
                        self.reload_on(n.as_deref());
                        self.note = format!("Pegado: {}", n.unwrap_or_default());
                    }
                    Err(e) => self.fail("pegar", e),
                }
            }
            Op::Trash => {
                let Some(p) = self.chosen() else { return };
                let Some(trash) = fileops::trash_dir() else {
                    self.note = "No hay papelera: falta HOME".into();
                    return;
                };
                match fileops::trash_into(std::path::Path::new(&p), &trash, files::now_secs()) {
                    Ok(_) => {
                        if self.held.as_ref().is_some_and(|(h, _)| *h == p) {
                            self.held = None;
                        }
                        self.reload_on(None);
                        self.note = format!("A la papelera: {}", name_of(&p));
                    }
                    Err(e) => self.fail("mover a la papelera", e),
                }
            }
        }
    }

    /// Enter while a name is being typed: do what it was typed for.
    pub(crate) fn finish_edit(&mut self) {
        let Some(edit) = self.edit.take() else { return };
        let name = self.field.trim().to_string();
        self.field = std::mem::take(&mut self.kept_filter);
        let r = match &edit {
            Edit::Rename(p) => fileops::rename(std::path::Path::new(p), &name).map(|_| "Renombrado"),
            Edit::NewDir => fileops::make_dir(std::path::Path::new(&self.dir), &name).map(|_| "Carpeta creada"),
        };
        match r {
            Ok(done) => {
                if let (Edit::Rename(p), Some((h, _))) = (&edit, self.held.as_mut()) {
                    if h == p {
                        *h = files::join_child(&self.dir, &name);
                    }
                }
                self.field.clear();
                self.reload_on(Some(&name));
                self.note = format!("{done}: {name}");
            }
            Err(e) => {
                // Keep typing: the name is still there to fix.
                self.kept_filter = std::mem::replace(&mut self.field, name);
                self.edit = Some(edit);
                self.note = format!("No se pudo: {e}");
                self.refilter();
            }
        }
    }

    /// Esc while a name is being typed.
    pub(crate) fn cancel_edit(&mut self) {
        if self.edit.take().is_some() {
            self.field = std::mem::take(&mut self.kept_filter);
            self.note.clear();
            self.refilter();
        }
    }
}

/// Paint the action menu under row `y` of the list at `lx`..`lx + lw`, and
/// return where each action can be clicked.
pub fn draw_menu(cv: &mut Canvas, st: &State, lx: i32, lw: i32, y: i32, bottom: i32) -> Vec<(Rect, Op)> {
    let Some(sel) = st.menu else { return Vec::new() };
    let p = st.pal;
    let hud = st.look == Look::Tecno;
    let rad = |r: i32| if hud { 0 } else { r };
    let mh = MENU_HEAD_H + OPS.len() as i32 * MENU_ROW_H + 8;
    let mx = lx + (lw - MENU_W).clamp(0, 40);
    // Below the row when it fits, above it when it does not.
    let my = if y + mh <= bottom { y } else { (y - ROW_H - mh).max(0) };
    cv.round_rect_a(mx - 1, my - 1, MENU_W + 2, mh + 2, rad(7), if hud { p.accent } else { p.border }, 0.9);
    cv.round_rect(mx, my, MENU_W, mh, rad(6), p.panel);
    if hud {
        cv.brackets(mx - 1, my - 1, MENU_W + 2, mh + 2, 8, 1, GOLD);
    }
    let what = st.chosen().map(|c| c.rsplit('/').next().unwrap_or("").to_string()).unwrap_or_else(|| "..".into());
    let head = files::elide_middle(&what, ((MENU_W - 24) / GLYPH_W) as usize);
    cv.text_bold(&head, mx + 12, my + (MENU_HEAD_H - GLYPH_H) / 2, if hud { GOLD } else { p.text });
    cv.hline(mx + 8, my + MENU_HEAD_H - 1, MENU_W - 16, p.border, 0.8);
    let mut hits = Vec::new();
    for (i, &op) in OPS.iter().enumerate() {
        let ry = my + MENU_HEAD_H + 4 + i as i32 * MENU_ROW_H;
        let c = op_colour(op, p);
        let ok = st.can(op);
        if i == sel {
            cv.round_rect_a(mx + 4, ry, MENU_W - 8, MENU_ROW_H - 2, rad(5), c, 0.16);
            if hud {
                cv.fill_rect_a(mx + 4, ry, 2, MENU_ROW_H - 2, c, 1.0);
            }
        }
        // The action's colour, as a mark; dim when there is nothing to do it to.
        let (mark, text) = if ok { (c, if i == sel { p.text } else { p.text }) } else { (p.dim, p.dim) };
        cv.fill_rect_a(mx + 14, ry + (MENU_ROW_H - 2 - 8) / 2, 8, 8, mark, if ok { 1.0 } else { 0.5 });
        let ty = ry + (MENU_ROW_H - 2 - GLYPH_H) / 2;
        let label = if hud { op_label(op).to_uppercase() } else { op_label(op).to_string() };
        cv.text(&label, mx + 32, ty, if i == sel && ok { c } else { text });
        let k = op_key(op);
        cv.text(k, mx + MENU_W - 12 - Canvas::text_width(k), ty, p.dim);
        hits.push(((mx + 4, ry, MENU_W - 8, MENU_ROW_H - 2), op));
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_its_own_colour_and_key() {
        let p = palette(Look::Tecno);
        for (i, &a) in OPS.iter().enumerate() {
            for &b in &OPS[..i] {
                assert_ne!(op_colour(a, p), op_colour(b, p), "{a:?} {b:?}");
                assert_ne!(op_key(a), op_key(b));
            }
            let label_w = Canvas::text_width(&op_label(a).to_uppercase());
            assert!(32 + label_w + 12 + Canvas::text_width(op_key(a)) + 12 <= MENU_W, "{a:?}");
        }
        for (k, op) in [(60, Op::Rename), (63, Op::Copy), (64, Op::Cut), (110, Op::Paste), (65, Op::NewDir), (111, Op::Trash), (66, Op::Trash)] {
            assert_eq!(op_for_key(k), Some(op));
        }
        assert_eq!(op_for_key(33), None, "a letter is the filter's");
    }
}
