//! lunarfile — Eclipse OS's file browser, and the file chooser other programs
//! can call.
//!
//! Super+E used to run `eclipse-files`, a script that looked for `mc`, `nnn`,
//! `lf`, `ranger` or `vifm` and, finding none of them in this image, opened a
//! shell in `$HOME`. And there was no graphical way to pick a file at all, so
//! "open file" in any application had nothing to open.
//!
//! Dolphin, Nautilus and Thunar are all out for the reason the rest of this
//! crate exists: every one of them is a GTK or Qt application that registers on
//! the session D-Bus and pulls in gdk-pixbuf, fontconfig and a theme engine. So
//! this is the same thing rebuilt on what the session does have — one static
//! musl binary over wlr-layer-shell + wl_shm, sharing `lunarbar`'s drawing
//! stack, icon cache and list navigation. No GTK, no Qt, no D-Bus.
//!
//! Modes:
//! - default: a centred browser. ↑/↓ or Tab to move, Enter or → to enter a
//!   directory (or open a file with `open::handler_for`), ← or Backspace to go
//!   up, type to filter, a filter starting with `.` reveals dotfiles, Esc
//!   closes. Clicking outside closes it. A rail of places (home, its usual
//!   folders, `/`, what is mounted) sits left of the list on a wide enough
//!   panel; each row shows its size and modification date, and a file's icon
//!   is coloured by kind.
//! - `--pick`: the same browser as a **file chooser**. Enter on a file prints
//!   its path and exits 0; Esc exits 1 having printed nothing. `--multiple`
//!   marks with Space and prints one path per line; `--save NAME` turns the
//!   field into a filename and prints the path it would be saved to; a name
//!   that already exists takes a second Enter. The choosers also have Cancel
//!   and Abrir/Guardar buttons in the footer. `--out F`
//!   writes the answer to `F` instead of stdout, which is what a caller that
//!   launched this from a keybind or a D-Bus service can actually read.
//! - `--dump PATH:WxH`: render the browser to a raw ARGB8888 file and exit, for
//!   offline verification with no compositor.
//!
//! `--pick` is the seam the XDG desktop portal's `FileChooser` backend needs:
//! `org.freedesktop.portal.FileChooser.OpenFile` is how GTK and Firefox ask for
//! a file, and a backend for it is one process that spawns this and reads the
//! `--out` file back. That backend is `eclipse-portald`
//! (`tools/eclipse-dbusd`), and it is a SERVICE: `eclipse-init` starts it with
//! no `WAYLAND_DISPLAY`, so the chooser cannot assume one -- see
//! `lunarbar::wl`.
//!
//! Env: `ECLIPSE_LOOK=kde|eclipse|win11` to override `/etc/eclipse/look`.

use std::os::fd::AsFd;

#[path = "lunarfile/actions.rs"]
mod actions;
use actions::{Edit, Op};
#[path = "lunarfile/job.rs"]
mod job;
use std::os::fd::AsRawFd;

use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::files::{self, Entry};
use lunarbar::fill_guard;
use lunarbar::i18n::{Key, Lang};
use lunarbar::keys::{
    key_char, BTN_LEFT, BTN_RIGHT, KEY_BACKSPACE_WL, KEY_INSERT_WL, KEY_DOWN_WL, KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL,
    KEY_LEFT_WL, KEY_PGDN_WL, KEY_PGUP_WL, KEY_RIGHT_WL, KEY_TAB_WL, KEY_UP_WL, WHEEL_NOTCH,
};
use lunarbar::list::{inside_rect, move_selection, row_at_geom, scroll_top, Step};
use lunarbar::look::Look;
use lunarbar::open;
use lunarbar::preview::{self, Body};
use lunarbar::proc::{self, free_buffer, map_shm_pool, spawn_detached, BUFFERS};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

// ── palette ──────────────────────────────────────────────────────────────────

/// The colours of one look, the same four-field shape `lunarrun` uses so the
/// chooser and the launcher are visibly the same desktop.
struct Palette {
    scrim: Rgb,
    scrim_a: f32,
    panel: Rgb,
    /// Field / row well, a shade darker than the panel.
    field: Rgb,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    sel: Rgb,
    /// Text on a selected row. Not always `text`: KDE selects with its light
    /// accent, and white on `#3daee9` is 2.3:1, which nobody can read.
    sel_text: Rgb,
    /// Text on an accent fill (the primary button, the mode chip).
    on_accent: Rgb,
    border: Rgb,
}

/// The overwrite warning, the same amber in every look: it has to read as
/// "careful" against all three panels and as nothing like the accent.
const WARN: Rgb = (0xe0, 0xa8, 0x5e);
const ON_WARN: Rgb = (0x1a, 0x10, 0x06);
/// Tecno's folders: the gold of the crescent, as mockup A marks them.
const GOLD: Rgb = (0xf2, 0xc1, 0x4e);

const PAL_KDE: Palette = Palette {
    scrim: (0x0d, 0x0f, 0x11),
    scrim_a: 0.45,
    panel: (0x2a, 0x2e, 0x32),
    field: (0x1b, 0x1e, 0x20),
    text: (0xfc, 0xfc, 0xfc),
    dim: (0x7f, 0x8c, 0x8d),
    accent: (0x3d, 0xae, 0xe9),
    sel: (0x3d, 0xae, 0xe9),
    sel_text: (0x0d, 0x14, 0x18),
    on_accent: (0x0d, 0x14, 0x18),
    border: (0x4d, 0x4d, 0x4d),
};

const PAL_ECLIPSE: Palette = Palette {
    scrim: (0x04, 0x07, 0x0e),
    scrim_a: 0.45,
    panel: (0x0b, 0x12, 0x20),
    field: (0x12, 0x21, 0x38),
    text: (0xff, 0xff, 0xff),
    dim: (0x6d, 0x7f, 0xa3),
    accent: (0x6e, 0xa8, 0xff),
    sel: (0x1f, 0x3a, 0x63),
    sel_text: (0xff, 0xff, 0xff),
    on_accent: (0x0b, 0x12, 0x20),
    border: (0x27, 0x5a, 0x9e),
};

/// Eclipse `tecno`: the instrument-panel look. Same cold family as
/// `PAL_ECLIPSE`, ground pushed nearly to black and the accent moved to a
/// signal cyan, so it matches the panel the user switched with it.
const PAL_TECNO: Palette = Palette {
    scrim: (0x03, 0x06, 0x0a),
    scrim_a: 0.45,
    panel: (0x08, 0x0e, 0x1a),
    field: (0x0a, 0x14, 0x23),
    text: (0xe6, 0xf1, 0xff),
    // Secondary text, not decoration: 6.8:1 on the panel ground.
    dim: (0x8a, 0xa4, 0xc2),
    accent: (0x38, 0xe1, 0xff),
    sel: (0x0c, 0x2a, 0x3a),
    sel_text: (0xe6, 0xf1, 0xff),
    on_accent: (0x04, 0x07, 0x0d),
    border: (0x1d, 0x35, 0x50),
};

const PAL_WIN11: Palette = Palette {
    scrim: (0x10, 0x10, 0x10),
    scrim_a: 0.40,
    panel: (0x2b, 0x2b, 0x2b),
    field: (0x1c, 0x1c, 0x1c),
    text: (0xff, 0xff, 0xff),
    dim: (0xc5, 0xc5, 0xc5),
    accent: (0x00, 0x78, 0xd4),
    sel: (0x00, 0x78, 0xd4),
    sel_text: (0xff, 0xff, 0xff),
    on_accent: (0xff, 0xff, 0xff),
    border: (0x3d, 0x3d, 0x3d),
};

fn palette(look: Look) -> &'static Palette {
    match look {
        Look::Win11 => &PAL_WIN11,
        Look::Kde => &PAL_KDE,
        Look::Eclipse => &PAL_ECLIPSE,
        Look::Tecno => &PAL_TECNO,
    }
}

// ── geometry ─────────────────────────────────────────────────────────────────

const PANEL_W: i32 = 1120;
const MIN_PANEL_W: i32 = 360;
/// The path header.
const HEAD_H: i32 = 32;
/// The filter / filename field.
const FIELD_H: i32 = 40;
/// The filter field drawn inside its `FIELD_H` slot: on the 4-px grid, like
/// every other measure here.
const FIELD_BOX: i32 = FIELD_H - 8;
const ROW_H: i32 = 32;
const PAD: i32 = 12;
/// Two lines: the key hints and whatever the last action had to say.
const FOOT_H: i32 = 40;
const ICON: i32 = 20;
/// The column titles above the rows (Nombre / Tamaño / Modificado). Dropped
/// on a panel too short to also show a few rows: see `cols_h`.
const COLS_H: i32 = 20;
/// The places rail, on a panel wide enough to give it up: see `rail_w`.
const RAIL_W: i32 = 168;
const RAIL_MIN_PANEL: i32 = 620;
const PLACE_H: i32 = 28;
/// The preview column, right of the list, on a panel wide enough to give it
/// up: see `preview_w`.
const PREVIEW_W: i32 = 272;
const PREVIEW_MIN_PANEL: i32 = 900;
/// The rows a panel with a preview is tall enough for, however few the
/// folder holds, so a picture or a text has somewhere to show.
const PREVIEW_ROWS: usize = 10;
/// The checkbox column `--multiple` puts in front of every row.
const CHECK_W: i32 = 20;
/// The footer's buttons in the choosers.
const BTN_H: i32 = 28;

/// Rows visible at once. More than the launcher's eight, because a directory is
/// longer than a filtered application list and scrolling a file browser one row
/// at a time is the thing that makes one unusable.
const MAX_ROWS: usize = 14;

/// A field is not a document: 256 characters is longer than any filename Linux
/// allows (255 bytes) and the cap exists so holding a key cannot grow a String
/// without bound.
/// The longest header drawn, in characters. The rest of the line is the path.
const MAX_TITLE: usize = 28;
const MAX_FIELD: usize = 256;

/// The size column, in characters, plus the gap before it.
const SIZE_COL: i32 = 9;
const COL_GAP: i32 = 16;
/// The date column, in characters: `files::short_date` is never wider. It is
/// one width per language (English says `yesterday 18:03`, Spanish `ayer
/// 18:03`), so it is a function and not a constant, and everything that
/// measures the row takes it as an argument.
fn date_col(lang: Lang) -> i32 {
    files::date_chars(lang) as i32
}
/// The narrowest a name may get before the date column is dropped for it.
const NAME_MIN: i32 = 16;

// ── what the browser is for ──────────────────────────────────────────────────

/// Why the browser is open, which is the only thing Enter on a file means
/// differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Super+E: Enter on a file opens it with its handler.
    Browse,
    /// `--pick`: Enter on a file answers with it.
    Open,
    /// `--pick --multiple`: Space marks, Enter answers with everything marked.
    OpenMany,
    /// `--pick --save NAME`: the field is a filename, Enter answers with
    /// `<dir>/<name>`, whether or not that file exists yet.
    Save,
}

impl Mode {
    /// Does this mode answer a caller, rather than launching programs?
    fn is_pick(self) -> bool {
        !matches!(self, Mode::Browse)
    }
}

/// One row of the list: the `..` link, or an entry of the current directory.
///
/// `..` is a row and not a separate key because it has to be clickable and has
/// to scroll with the list; making it index -1 or a flag on the selection is
/// how a file browser ends up opening the wrong file after a filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Parent,
    /// An index into the **unfiltered** listing.
    Entry(usize),
}

/// The rows to draw for a directory: `..` first unless this is the root, then
/// the visible entries in listing order.
fn rows_of(dir: &str, entries: &[Entry], filter: &str, show_hidden: bool) -> Vec<Row> {
    let mut out = Vec::new();
    // `..` survives a filter on purpose: a filter that matches nothing would
    // otherwise leave a panel with no way out but Esc, and typing a wrong
    // letter is how that happens.
    if files::parent_of(dir).is_some() {
        out.push(Row::Parent);
    }
    out.extend(
        files::visible(entries, filter, show_hidden)
            .into_iter()
            .map(Row::Entry),
    );
    out
}

/// Are dotfiles showing? `--hidden` says so for the whole session, and a filter
/// that starts with a dot says so for as long as it does.
///
/// Typing the dot is the gesture because there is no modifier to hang a
/// Ctrl+H on: resolving modifiers means libxkbcommon and a keymap fd (see
/// `keys`), and `.` is a key this client can already read.
fn show_hidden(filter: &str, always: bool) -> bool {
    always || filter.trim_start().starts_with('.')
}

// ── keys ─────────────────────────────────────────────────────────────────────

/// What a key press means. One table, so the browser and its tests agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Close,
    /// Enter / →: enter a directory, or open/answer with a file.
    Activate,
    /// Enter in `--save`: answer with the name in the field.
    ///
    /// It cannot be `Activate`, and that is the whole bug it fixes: in a
    /// populated directory something is always selected (`..` at the very
    /// least), so an `Activate` Enter navigated or copied a name into the
    /// field and the Save chooser **could never answer**. `→` still navigates,
    /// and a click on a file still offers its name, which is what a Save
    /// dialog does; Enter is the one key that means "this name, here".
    Commit,
    /// ←: up one directory, whatever is in the field.
    Up,
    /// Backspace: a character off the field, or up one directory when the field
    /// is empty.
    Rub,
    Move(i32, Step),
    /// Page Up / Page Down, in pages. How many rows that is cannot be decided
    /// here: it is `rows_shown`, which the last frame measured against the
    /// screen it actually got. Moving by the maximum on a short output jumped
    /// fourteen rows in a list showing three.
    Page(i32),
    /// Space in `--multiple`: mark or unmark the selected row.
    Mark,
    Type(char),
    /// F2, F5, F6, Insert, F7, Supr: an action on the selection.
    Op(Op),
    /// The Menu key: the action menu.
    Menu,
    Ignore,
}

fn key_action(key: u32, mode: Mode) -> Action {
    match key {
        KEY_ESC_WL => Action::Close,
        KEY_ENTER_WL | KEY_KPENTER_WL if mode == Mode::Save => Action::Commit,
        KEY_ENTER_WL | KEY_KPENTER_WL | KEY_RIGHT_WL => Action::Activate,
        KEY_LEFT_WL => Action::Up,
        KEY_BACKSPACE_WL => Action::Rub,
        KEY_UP_WL => Action::Move(-1, Step::Wrap),
        KEY_DOWN_WL | KEY_TAB_WL => Action::Move(1, Step::Wrap),
        KEY_PGUP_WL => Action::Page(-1),
        KEY_PGDN_WL => Action::Page(1),
        // Only the browser changes files; a chooser answers a caller.
        k if mode == Mode::Browse && actions::op_for_key(k).is_some() => {
            actions::op_for_key(k).map_or(Action::Ignore, Action::Op)
        }
        k if mode == Mode::Browse && actions::is_menu_key(k) => Action::Menu,
        k => match key_char(k) {
            // Space marks when marking is what Space is for. In every other
            // mode it is a character, because filenames have spaces in them and
            // a space that silently did nothing would be the one key the filter
            // refused.
            Some(' ') if mode == Mode::OpenMany => Action::Mark,
            Some(c) => Action::Type(c),
            None => Action::Ignore,
        },
    }
}

/// The footer's buttons, which only the choosers have: a chooser opened from
/// an application is driven with the mouse as often as with the keyboard, and
/// a pointer-only user had no way to say "this one" or "never mind".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    Cancel,
    /// Abrir / Guardar / Reemplazar: what Enter would do.
    Primary,
}

type Rect = (i32, i32, i32, i32);

/// Where one frame put everything a click can land on, handed back by
/// `draw_browser` as one value so that hit testing is never told one frame's
/// geometry and another frame's row count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Frame {
    panel: Rect,
    /// The list column. Rows are hittable only inside it, not under the rail.
    list: Rect,
    row0_y: i32,
    rows_shown: usize,
    /// One rect per place the rail drew, in `State::places` order.
    places: Vec<Rect>,
    /// The action menu's rows, when it is open.
    menu: Vec<(Rect, Op)>,
    buttons: Vec<(Rect, Button)>,
}

// ── state ────────────────────────────────────────────────────────────────────

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: Vec<wl_output::WlOutput>,

    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    width: u32,
    height: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    /// A frame that could not be drawn because the compositor held both
    /// buffers. `render` used to say "the next Release repaints" and nothing
    /// did: the `Release` handler only cleared `busy`, so after a burst of
    /// more than two updates the LAST one -- the keystroke the user is
    /// looking at -- could stay invisible until the next event.
    dirty: bool,
    next: usize,
    generation: u64,
    configured: bool,

    pal: &'static Palette,
    look: Look,
    /// The language every string on the panel comes from, read ONCE at start.
    ///
    /// Once, and not per frame: `Lang::current` reads `/etc/eclipse/locale`
    /// and `$LANG`, and a window that re-read them while it was open could
    /// draw half its labels in one language if `eclipse-locale` ran between
    /// two frames.
    lang: Lang,
    mode: Mode,
    /// Where `--pick` writes its answer, or None for stdout.
    out: Option<String>,
    /// `/etc/eclipse/openers`, read once.
    openers: Vec<(String, String)>,
    /// Show dotfiles whatever the field says.
    always_hidden: bool,
    /// The header, when the caller gave one.
    title: Option<String>,

    dir: String,
    entries: Vec<Entry>,
    /// What went wrong with the last action, shown on the footer's second line.
    note: String,
    /// The filter, or in `Save` the filename.
    field: String,
    rows: Vec<Row>,
    /// Absolute paths marked in `--multiple`, in the order they were marked.
    marked: Vec<String>,
    /// What `--pick` will print. Set by `answer`, which also ends the loop.
    picked: Vec<String>,

    /// The places rail, read once at start.
    places: Vec<files::Place>,
    /// `--save`: the existing file Enter has already been warned about, so the
    /// next Enter on the same name replaces it. Anything typed, or a move,
    /// disarms it.
    confirm: Option<String>,

    sel: usize,
    top: usize,
    hover: Option<usize>,
    ptr: (f64, f64),
    panel: Rect,
    list_rect: Rect,
    place_rects: Vec<Rect>,
    buttons: Vec<(Rect, Button)>,
    /// The action menu, open on this row of `actions::OPS`.
    menu: Option<usize>,
    menu_rects: Vec<(Rect, Op)>,
    /// A name being typed for a rename or a new folder; the filter it put
    /// aside comes back when it is done.
    edit: Option<Edit>,
    kept_filter: String,
    /// What was copied (false) or cut (true), waiting to be pasted.
    held: Option<(String, bool)>,
    /// The paste running in the background, if one is.
    job: Option<job::Job>,
    row0_y: i32,
    /// How many rows the LAST frame actually drew, which on a short output is
    /// fewer than `MAX_ROWS`. Hit testing and the wheel both have to use this
    /// and not the cap: a click below the last drawn row but still inside the
    /// panel would otherwise resolve to a row that is not on the screen, and
    /// open a file the user cannot see.
    rows_shown: usize,
    scroll_acc: f64,
    done: bool,
    /// What the preview column shows, read once per selection: the entry
    /// it is of and the box its picture was scaled for (see `preview`).
    preview: std::cell::RefCell<Option<(String, (u32, u32), preview::Preview)>>,
}

impl State {
    /// Read `dir` into the listing and rebuild the rows. A directory that
    /// cannot be read leaves the browser where it was, with a note: walking into
    /// an unreadable directory and showing it empty is a file browser that lies.
    fn chdir(&mut self, dir: &str) {
        let dir = files::normalize_dir(dir);
        match files::read_dir_sorted(&dir) {
            Ok(entries) => {
                self.dir = dir;
                self.entries = entries;
                self.note.clear();
                self.confirm = None;
                // The field is a filter of THIS directory, so it does not
                // survive the move -- except in Save, where it is the filename
                // the user is still typing.
                if self.mode != Mode::Save {
                    self.field.clear();
                }
                self.sel = 0;
                self.top = 0;
                self.hover = None;
                self.scroll_acc = 0.0;
                self.refilter();
            }
            Err(e) => {
                self.note = format!("{dir}: {e}");
                self.refilter();
            }
        }
    }

    fn refilter(&mut self) {
        let hidden = show_hidden(&self.field, self.always_hidden);
        // In Save the field is a filename, not a filter: narrowing the listing
        // as the user types a NEW name would empty the panel by the third
        // letter, since nothing in the directory is called that yet.
        let filter = if self.mode == Mode::Save || self.edit.is_some() {
            ""
        } else {
            &self.field
        };
        self.rows = rows_of(&self.dir, &self.entries, filter, hidden);
        let (sel, top) = move_selection(
            self.sel,
            self.top,
            self.rows.len(),
            0,
            Step::Clamp,
            self.rows_shown,
        );
        self.sel = sel;
        self.top = top;
    }

    fn row(&self, row: usize) -> Option<Row> {
        self.rows.get(row).copied()
    }

    fn entry(&self, row: usize) -> Option<&Entry> {
        match self.row(row)? {
            Row::Parent => None,
            Row::Entry(i) => self.entries.get(i),
        }
    }

    /// The absolute path a row stands for.
    fn path_of(&self, row: usize) -> Option<String> {
        match self.row(row)? {
            Row::Parent => files::parent_of(&self.dir),
            Row::Entry(i) => Some(files::join_child(&self.dir, &self.entries.get(i)?.name)),
        }
    }

    fn move_sel(&mut self, delta: i32, step: Step) {
        let (sel, top) = move_selection(
            self.sel,
            self.top,
            self.rows.len(),
            delta,
            step,
            self.rows_shown,
        );
        self.sel = sel;
        self.top = top;
    }

    fn go_up(&mut self) {
        if let Some(up) = files::parent_of(&self.dir) {
            let leaving = self.dir.clone();
            self.chdir(&up);
            // Land on the directory just left rather than at the top: climbing
            // out of the twentieth subdirectory and being dropped at row 0 loses
            // the user's place every single time.
            let name = leaving.rsplit('/').next().unwrap_or_default().to_string();
            if let Some(r) = self
                .rows
                .iter()
                .position(|r| matches!(r, Row::Entry(i) if self.entries[*i].name == name))
            {
                let (sel, top) =
                    move_selection(r, 0, self.rows.len(), 0, Step::Clamp, self.rows_shown);
                self.sel = sel;
                self.top = top;
            }
        }
    }

    /// Enter, → or a click on a row.
    fn activate(&mut self, row: usize) {
        let Some(kind) = self.row(row) else {
            // Save answers even with nothing selected: the name in the field is
            // the whole point, and a new file matches no row.
            if self.mode == Mode::Save {
                self.save_answer();
            }
            return;
        };
        if kind == Row::Parent {
            self.go_up();
            return;
        }
        let is_dir = self.entry(row).map(|e| e.is_dir).unwrap_or(false);
        let Some(path) = self.path_of(row) else {
            return;
        };
        if is_dir {
            self.chdir(&path);
            return;
        }
        match self.mode {
            Mode::Browse => self.launch(&path),
            Mode::Open => self.answer(vec![path]),
            Mode::OpenMany => {
                // Enter with nothing marked answers with what is selected,
                // which is what a chooser does when one file is all you wanted.
                let picked = if self.marked.is_empty() {
                    vec![path]
                } else {
                    std::mem::take(&mut self.marked)
                };
                self.answer(picked);
            }
            // Clicking an existing file in Save puts its name in the field
            // (overwrite that one), rather than answering behind the user's
            // back with a path they only pointed at.
            // The click is the user pointing at THAT file, so the warning is
            // given and armed at once: one Enter replaces it.
            Mode::Save => {
                if let Some(name) = self.entry(row).map(|e| e.name.clone()) {
                    self.note = format!("{name} {}", self.lang.s(Key::file_exists_enter));
                    self.field = name;
                    self.confirm = Some(path);
                    self.refilter();
                }
            }
        }
    }

    /// `--save`: the path the field names in this directory.
    /// Enter in `--save`: the name in the field is the answer.
    ///
    /// With nothing typed there is nothing to save, and then Enter does what
    /// it does everywhere else -- go into the directory under the cursor --
    /// so that reaching a folder and typing a name is one uninterrupted
    /// gesture. With a name typed, Enter always answers, whatever is selected:
    /// a Save dialog that needed the selection cleared first could not be
    /// driven at all.
    fn commit(&mut self) {
        if self.field.trim().is_empty() {
            let sel = self.sel;
            let into_dir = matches!(self.row(sel), Some(Row::Parent))
                || self.entry(sel).map(|e| e.is_dir).unwrap_or(false);
            if into_dir {
                self.activate(sel);
                return;
            }
        }
        self.save_answer();
    }

    fn save_answer(&mut self) {
        let name = self.field.trim();
        if name.is_empty() {
            self.note = self.lang.s(Key::file_type_a_name).to_string();
            return;
        }
        let path = files::join_child(&self.dir, name);
        // An existing file is replaced only on a second Enter. A Save dialog
        // that overwrote `informe.pdf` because the user typed the name of the
        // one already there is how a week of work goes.
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => {
                self.note = format!("{name} {}", self.lang.s(Key::file_is_a_folder));
                return;
            }
            Ok(_) if self.confirm.as_deref() != Some(path.as_str()) => {
                self.note = format!("{name} {}", self.lang.s(Key::file_exists_enter_again));
                self.confirm = Some(path);
                return;
            }
            _ => {}
        }
        self.answer(vec![path]);
    }

    /// Is the next Enter going to replace an existing file?
    fn armed(&self) -> bool {
        self.mode == Mode::Save && self.confirm.is_some()
    }

    /// A click on a place in the rail.
    fn go_place(&mut self, i: usize) {
        if let Some(path) = self.places.get(i).map(|p| p.path.clone()) {
            self.chdir(&path);
        }
    }

    /// A click on one of the footer's buttons.
    fn press(&mut self, b: Button) {
        match b {
            Button::Cancel => self.done = true,
            Button::Primary if self.mode == Mode::Save => self.commit(),
            Button::Primary => {
                let sel = self.sel;
                self.activate(sel);
            }
        }
    }

    fn toggle_mark(&mut self, row: usize) {
        // Only a file is markable: a directory in a multi-select is how a
        // chooser ends up answering with something the caller cannot open.
        if self.entry(row).map(|e| e.is_dir).unwrap_or(true) {
            return;
        }
        let Some(path) = self.path_of(row) else {
            return;
        };
        if let Some(i) = self.marked.iter().position(|p| *p == path) {
            self.marked.remove(i);
        } else if !fill_guard::try_push_bounded(&mut self.marked, path, MAX_MARKED) {
            self.note = format!(
                "{} {MAX_MARKED} {}",
                self.lang.s(Key::file_at_most),
                self.lang.s(Key::file_n_files)
            );
        }
    }

    /// Open a file with whatever `/etc/eclipse/openers` or the built-in table
    /// says, and say so on the panel when nothing does.
    fn launch(&mut self, path: &str) {
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some(template) = open::handler_for(name, &self.openers) else {
            self.note = match open::extension_of(name) {
                Some(ext) => self.lang.s(Key::file_no_opener).replace("{}", &ext),
                None => self.lang.s(Key::file_no_extension).to_string(),
            };
            return;
        };
        // Checked BEFORE spawning: `spawn_detached` is a double-fork into
        // `sh -c`, so a missing program is an `exit 127` in a process nobody is
        // watching -- the file would appear to open into nothing at all.
        if let Some(prog) = open::program_of(&template) {
            if !lunarbar::apps::executable_exists(prog) {
                self.note = self.lang.s(Key::file_not_installed).replace("{}", prog);
                return;
            }
        }
        spawn_detached(&open::open_command(path, &template));
        // The browser closes behind the launch, like every desktop's does.
        self.done = true;
    }

    /// `--pick` is answered: remember the paths and end the loop. Writing
    /// happens in `main`, after the Wayland teardown, so the caller does not
    /// read the answer while the overlay is still on screen.
    fn answer(&mut self, paths: Vec<String>) {
        self.picked = paths;
        self.done = true;
    }

    fn map_overlay(&mut self, qh: &QueueHandle<State>) {
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        if self.surface.is_some() {
            return;
        }
        let surface = comp.create_surface(qh, ());
        let layer = ls.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Overlay,
            "filechooser".into(),
            qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_size(0, 0);
        // -1: the FULL output, panel included. With the default 0 the
        // compositor hands back the usable area and the scrim would stop at the
        // taskbar, leaving its clicks live while this surface holds the keyboard.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        self.surface = Some(surface);
        self.layer = Some(layer);
    }

    fn configure(&mut self, qh: &QueueHandle<State>, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let Some((w, h)) = surface_size(w, h) else {
            eprintln!("lunarfile: {w}x{h} is not renderable; skipping");
            return;
        };
        if self.configured && self.width == w && self.height == h {
            self.render();
            return;
        }
        let Some(geom) = proc::pool_geometry(w, h) else {
            eprintln!("lunarfile: {w}x{h} does not fit a wl_shm pool; skipping");
            return;
        };
        let (total, stride, frame_size) = (geom.total, geom.stride, geom.frame_size);
        let Some((map, fd)) = map_shm_pool(total, "lunarfile") else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let mk = |i: usize| {
            pool.create_buffer(
                (i * frame_size) as i32,
                w as i32,
                h as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (i, generation),
            )
        };
        let buffers = [Some(mk(0)), Some(mk(1))];
        pool.destroy();

        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        self.width = w;
        self.height = h;
        self.map = map;
        self.map_len = total;
        self.buffers = buffers;
        self.pool_replaced();
        self.configured = true;
        self.render();
    }

    /// The buffer this frame goes into, booking the debt when there is none.
    ///
    /// The two halves have to move together: `None` here means the frame is
    /// owed, and `buffer_released` is the only thing that pays it. Taking a
    /// buffer without clearing `dirty` would leave a debt that is paid twice.
    fn claim_buffer(&mut self) -> Option<usize> {
        let i = free_buffer(self.busy, self.next);
        self.dirty = i.is_none();
        i
    }

    /// A new `wl_shm` pool replaced the old one.
    ///
    /// The frame that was owed belonged to the pool that just went away, and
    /// the indices in `busy` point into the new buffers, so both are reset
    /// together; the caller draws the first frame of the new pool.
    fn pool_replaced(&mut self) {
        self.busy = [false; BUFFERS];
        self.dirty = false;
        self.next = 0;
    }

    /// A buffer came back from the compositor.
    ///
    /// Returns whether a frame was owed, which is the whole decision: before
    /// this existed `render` set `dirty` and nobody ever looked at it again,
    /// so a frame dropped while both buffers were held stayed dropped and the
    /// panel showed stale rows until the next key.
    fn buffer_released(&mut self, i: usize, generation: u64) -> bool {
        // A buffer of a previous pool: its index means nothing now.
        if generation != self.generation || i >= BUFFERS {
            return false;
        }
        self.busy[i] = false;
        if !self.dirty {
            return false;
        }
        self.render();
        true
    }

    fn render(&mut self) {
        if !self.configured || self.map.is_null() {
            return;
        }
        let Some(surface) = self.surface.clone() else {
            return;
        };
        let Some(i) = self.claim_buffer() else {
            return;
        };
        let (w, h) = (self.width as usize, self.height as usize);
        let frame_size = w * h * 4;
        let Some(mut cv) = Canvas::try_new(w, h) else {
            return;
        };
        let f = draw_browser(&mut cv, w, h, self);
        self.take_frame(f);
        let data: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(self.map.add(i * frame_size), frame_size) };
        if !cv.blit_argb(data) {
            return;
        }
        if let Some(b) = &self.buffers[i] {
            surface.attach(Some(b), 0, 0);
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
            self.busy[i] = true;
            self.next = 1 - i;
        }
    }

    /// Keep what the frame just drawn says a click can land on.
    fn take_frame(&mut self, f: Frame) {
        self.panel = f.panel;
        self.list_rect = f.list;
        self.row0_y = f.row0_y;
        self.rows_shown = f.rows_shown;
        self.place_rects = f.places;
        self.buttons = f.buttons;
        self.menu_rects = f.menu;
    }

    fn menu_at(&self, x: f64, y: f64) -> Option<usize> {
        self.menu_rects.iter().position(|(r, _)| inside_rect(*r, x, y))
    }

    fn button_at(&self, x: f64, y: f64) -> Option<Button> {
        self.buttons
            .iter()
            .find(|(r, _)| inside_rect(*r, x, y))
            .map(|(_, b)| *b)
    }

    fn place_at(&self, x: f64, y: f64) -> Option<usize> {
        self.place_rects.iter().position(|r| inside_rect(*r, x, y))
    }

    fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        row_at_geom(
            self.list_rect,
            self.row0_y,
            ROW_H,
            self.rows_shown,
            self.top,
            self.rows.len(),
            x,
            y,
        )
    }

    fn inside_panel(&self, x: f64, y: f64) -> bool {
        inside_rect(self.panel, x, y)
    }
}

/// At most this many files in one `--multiple` answer. A chooser that let a
/// held-down Space mark every file on the disk would hand its caller a command
/// line it cannot exec.
const MAX_MARKED: usize = 1024;

impl Drop for State {
    fn drop(&mut self) {
        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        if let Some(l) = self.layer.take() {
            l.destroy();
        }
        if let Some(s) = self.surface.take() {
            s.destroy();
        }
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// The panel rect `(x, y, w, h)` for an output of `w` x `h` showing `rows`.
///
/// Centred both ways, unlike the launcher's upper third: a file browser is
/// where the user's attention is for as long as it is open, and it is tall
/// enough that an upper-third anchor would hang it off the bottom of a 720p
/// screen.
fn panel_rect(w: i32, h: i32, rows: usize) -> (i32, i32, i32, i32) {
    // Clamp to the output AFTER the minimum: on an output narrower than
    // MIN_PANEL_W the panel would otherwise come out wider than the screen and
    // `x` would go negative. `clamp` is not used because its floor and ceiling
    // cross over in exactly that case, and it panics then.
    let roomy = PANEL_W.min(w - 96).max(MIN_PANEL_W);
    let pw = roomy.min(w.max(1));
    let least = if preview_w(pw) > 0 { PREVIEW_ROWS } else { 1 };
    let shown = rows.clamp(least, MAX_ROWS) as i32;
    let want = PAD + HEAD_H + FIELD_H + PAD + COLS_H + shown * ROW_H + FOOT_H + PAD;
    // A short output caps the height, so the panel never hangs off an edge.
    let ph = want.min((h - 48).max(PAD * 2 + HEAD_H + FIELD_H + ROW_H + FOOT_H));
    let ph = ph.min(h.max(1));
    let px = (w - pw) / 2;
    let py = ((h - ph) / 2).max(0);
    (px, py, pw, ph)
}

/// The column titles' height in a panel of height `ph`: there when a few rows
/// still fit under them, gone when they would cost the only row on screen.
fn cols_h(ph: i32) -> i32 {
    let bare = PAD + HEAD_H + FIELD_H + PAD + FOOT_H + PAD;
    if ph - bare - COLS_H >= 3 * ROW_H {
        COLS_H
    } else {
        0
    }
}

/// How many rows actually fit in a panel of height `ph`, which on a short
/// output is fewer than `MAX_ROWS`.
fn rows_fitting(ph: i32) -> usize {
    let room = ph - (PAD + HEAD_H + FIELD_H + PAD + cols_h(ph) + FOOT_H + PAD);
    if room < ROW_H {
        return 1;
    }
    ((room / ROW_H) as usize).min(MAX_ROWS)
}

/// The places rail's width in a panel `pw` wide: none on a narrow one, where
/// the names would be what paid for it.
fn rail_w(pw: i32) -> i32 {
    if pw >= RAIL_MIN_PANEL {
        RAIL_W
    } else {
        0
    }
}

/// The preview column's width in a panel `pw` wide: none on a narrow one,
/// where the list would be what paid for it.
fn preview_w(pw: i32) -> i32 {
    if pw >= PREVIEW_MIN_PANEL {
        PREVIEW_W
    } else {
        0
    }
}

/// The path the preview is of: the selected entry, or on `..` (and in an
/// empty folder) the folder itself, so the column says what is here.
fn selected_path(st: &State) -> Option<String> {
    match st.rows.get(st.sel) {
        Some(Row::Entry(i)) => Some(files::join_child(&st.dir, &st.entries.get(*i)?.name)),
        _ => Some(st.dir.clone()),
    }
}

/// Paint the preview column at `x`, `y`, `w` x `h`: the selected entry's
/// name, what it holds in a framed box, and its details under it.
fn draw_preview(cv: &mut Canvas, st: &State, x: i32, y: i32, w: i32, h: i32) {
    let p = st.pal;
    let hud = st.look == Look::Tecno;
    let rad = |r: i32| if hud { 0 } else { r };
    cv.text(&cased(st, Key::file_preview), x + 8, y + 2, p.dim);
    let line = GLYPH_H + 4;
    let info_h = 4 * line + 4;
    let (bx, by) = (x, y + GLYPH_H + 10 + line + 4);
    let (bw, bh) = (w, (y + h - info_h - 8 - by).max(line));
    let chars = |px: i32| ((px / GLYPH_W).max(1)) as usize;
    let Some(path) = selected_path(st) else { return };
    let want = ((bw - 16).max(1) as u32, (bh - 16).max(1) as u32);
    {
        let mut cache = st.preview.borrow_mut();
        if !cache.as_ref().is_some_and(|(at, b, _)| *at == path && *b == want) {
            *cache = Some((path.clone(), want, preview::of(&path, want.0, want.1)));
        }
    }
    let cache = st.preview.borrow();
    let Some((_, _, pv)) = cache.as_ref() else { return };

    // The name, the way the list writes it.
    let nc = if pv.is_dir && hud { GOLD } else { p.text };
    cv.text_bold(&files::elide_middle(&pv.name, chars(w - 16)), x + 8, y + GLYPH_H + 10, nc);

    // The box.
    cv.round_rect(bx, by, bw, bh, rad(6), p.border);
    cv.round_rect(bx + 1, by + 1, bw - 2, bh - 2, rad(5), p.field);
    if hud {
        cv.brackets(bx, by, bw, bh, 6, 1, p.accent);
    }
    let (ix, iy) = (bx + 8, by + 8);
    let rows_fit = ((bh - 16) / (GLYPH_H + 2)).max(1) as usize;
    match &pv.body {
        Body::Image { pm, .. } => {
            let px = bx + (bw - pm.width() as i32) / 2;
            let py = by + (bh - pm.height() as i32) / 2;
            cv.pixmap(px, py, pm);
        }
        Body::Text { lines, more } => {
            let n = lines.len().min(rows_fit);
            for (k, l) in lines.iter().take(n).enumerate() {
                let s: String = l.chars().take(chars(bw - 16)).collect();
                let last = k + 1 == n && (*more || lines.len() > n);
                let c = if last { p.dim } else { p.text };
                cv.text(if last { "..." } else { &s }, ix, iy + k as i32 * (GLYPH_H + 2), c);
            }
        }
        Body::Dir { names, dirs, files: nf, .. } => {
            let total = dirs + nf;
            let n = names.len().min(rows_fit);
            for (k, (name, d)) in names.iter().take(n).enumerate() {
                let ly = iy + k as i32 * (GLYPH_H + 2);
                let last = k + 1 == n && total > n;
                if last {
                    cv.text(&format!("+{} {}", total - k, st.lang.s(Key::file_more)), ix, ly, p.dim);
                    break;
                }
                let c = if *d { if hud { GOLD } else { p.accent } } else { p.text };
                if *d {
                    cv.triangle_h(ix, ly + 4, 7, false, c);
                }
                cv.text(&files::elide_middle(name, chars(bw - 32)), ix + 12, ly, c);
            }
            if names.is_empty() {
                let s = st.lang.s(if total == 0 { Key::file_dir_empty } else { Key::file_dir_hidden_only });
                cv.text(s, bx + (bw - Canvas::text_width(s)) / 2, by + (bh - GLYPH_H) / 2, p.dim);
            }
        }
        Body::Plain | Body::Unreadable => {
            let s = 40;
            let c = kind_color(pv.kind, p);
            let cy = by + (bh - s - GLYPH_H - 12) / 2;
            file_icon(cv, bx + (bw - s) / 2, cy, s, c);
            let label = st.lang.s(if matches!(pv.body, Body::Unreadable) {
                Key::file_unreadable
            } else {
                Key::file_no_preview
            });
            let lc = if matches!(pv.body, Body::Unreadable) { WARN } else { p.dim };
            cv.text(label, bx + (bw - Canvas::text_width(label)) / 2, cy + s + 12, lc);
        }
    }

    // The details: what it is and weighs, what it holds or measures, when
    // it changed, who may touch it. Permissions keep their case in tecno:
    // `RW-R--R--` is not how anyone reads them.
    let plural = |n: usize, one: Key, many: Key| {
        format!("{n} {}", st.lang.s(if n == 1 { one } else { many }))
    };
    let mut info: Vec<(String, bool)> = Vec::new();
    match &pv.body {
        Body::Dir { dirs, files: nf, bytes, more, .. } => {
            info.push((
                format!("{} · {}", st.lang.s(Key::file_folder), files::human_size(*bytes, st.lang)),
                true,
            ));
            let plus = if *more { "+" } else { "" };
            info.push((
                format!(
                    "{}{plus} · {}",
                    plural(*dirs, Key::file_n_folder, Key::file_n_folders),
                    plural(*nf, Key::file_n_file, Key::file_n_files)
                ),
                true,
            ));
        }
        Body::Image { w, h, .. } => {
            info.push((format!(
            "{} · {}",
            preview::kind_label(pv.kind, pv.is_dir, st.lang),
            files::human_size(pv.size, st.lang)
        ), true));
            info.push((format!("{w} × {h} px"), true));
        }
        _ => info.push((format!(
            "{} · {}",
            preview::kind_label(pv.kind, pv.is_dir, st.lang),
            files::human_size(pv.size, st.lang)
        ), true)),
    }
    if let Some(t) = pv.mtime {
        let now = files::now_secs();
        info.push((
            format!(
                "{} {}",
                st.lang.s(Key::file_col_modified),
                files::short_date(t, now, files::local_offset(now), st.lang)
            ),
            true,
        ));
    }
    let mut perm = pv.mode.clone().unwrap_or_default();
    if let Some(l) = &pv.link {
        perm = format!("{perm} → {l}");
    }
    info.push((perm, false));
    let iy = y + h - info_h;
    for (k, (s, caps)) in info.iter().enumerate() {
        let s = files::elide_middle(s, chars(w - 16));
        let s = if hud && *caps { s.to_uppercase() } else { s };
        cv.text(&s, x + 8, iy + k as i32 * line, if k == 0 { p.text } else { p.dim });
    }
}

/// Does a list column `avail` px wide (after the icon) show the date column?
fn shows_dates(avail: i32, lang: Lang) -> bool {
    avail - (SIZE_COL + date_col(lang)) * GLYPH_W - 2 * COL_GAP >= NAME_MIN * GLYPH_W
}

/// How many characters of a name fit beside the size column, and beside the
/// date column too when `dates`.
fn name_chars(avail: i32, dates: bool, lang: Lang) -> usize {
    let date = if dates {
        date_col(lang) * GLYPH_W + COL_GAP
    } else {
        0
    };
    let room = avail - (SIZE_COL * GLYPH_W) - COL_GAP - date;
    if room > 0 {
        ((room / GLYPH_W).max(1)) as usize
    } else {
        (((avail - 8) / GLYPH_W).max(1)) as usize
    }
}

/// The colour of a file's icon: one per kind, the same in every look, chosen
/// to read on all three dark panels.
fn kind_color(kind: files::Kind, p: &Palette) -> Rgb {
    match kind {
        files::Kind::Image => (0x4f, 0xb5, 0xa5),
        files::Kind::Document => (0xe0, 0x87, 0x5e),
        files::Kind::Archive => (0xd6, 0xb2, 0x5c),
        files::Kind::Disk => (0xb4, 0x8c, 0xf0),
        files::Kind::Media => (0xe0, 0x7a, 0xb0),
        files::Kind::Code => (0x8f, 0xd1, 0x8a),
        files::Kind::Other => p.dim,
    }
}

/// A folder: a tab and a body. Drawn rather than themed, because the icon
/// cache only has application icons and a directory has no .desktop entry.
fn folder_icon(cv: &mut Canvas, x: i32, y: i32, s: i32, c: Rgb) {
    let body_h = s * 3 / 4;
    cv.round_rect(x, y + s - body_h - 2, s * 2 / 5, 4, 2, c);
    cv.round_rect(x, y + s - body_h, s, body_h, 3, c);
}

/// A file: a page, upright, in its kind's colour.
fn file_icon(cv: &mut Canvas, x: i32, y: i32, s: i32, c: Rgb) {
    cv.round_rect(x + s / 6, y, s - s / 3, s, 3, c);
}

/// What the header chip says, in the modes that answer a caller.
fn mode_chip(st: &State) -> Option<&'static str> {
    match st.mode {
        Mode::Browse => None,
        Mode::Open | Mode::OpenMany => Some(st.lang.s(Key::file_chip_pick)),
        Mode::Save => Some(st.lang.s(Key::file_chip_save)),
    }
}

/// A string of the table as THIS look writes it: tecno is a readout and puts
/// its headings in capitals, every other look leaves them as they are.
///
/// One function rather than a second row per heading: the capitals are a
/// property of the look, not of the language, and `TAMAÑO` is what
/// `Tamaño.to_uppercase()` gives in Rust.
fn cased(st: &State, key: Key) -> String {
    let s = st.lang.s(key);
    if st.look == Look::Tecno {
        s.to_uppercase()
    } else {
        s.to_string()
    }
}

/// The primary button's label: what Enter would do right now.
fn primary_label(st: &State) -> String {
    let s = |k: Key| st.lang.s(k).to_string();
    match st.mode {
        Mode::Browse => String::new(),
        Mode::Open => s(Key::file_btn_open),
        Mode::OpenMany if st.marked.is_empty() => s(Key::file_btn_open),
        Mode::OpenMany => format!("{} {}", s(Key::file_btn_open), st.marked.len()),
        Mode::Save if st.armed() => s(Key::file_btn_replace),
        Mode::Save => s(Key::file_btn_save),
    }
}

/// Paint the scrim and the centred browser, and say where everything went.
fn draw_browser(cv: &mut Canvas, w: usize, h: usize, st: &State) -> Frame {
    let p = st.pal;
    // Tecno is mockup A: square plates, caps labels, gold folders.
    let hud = st.look == Look::Tecno;
    let rad = |r: i32| if hud { 0 } else { r };
    cv.fill_rect_a(0, 0, w as i32, h as i32, p.scrim, p.scrim_a);

    let (px, py, pw, ph) = panel_rect(w as i32, h as i32, st.rows.len());
    cv.round_rect_a(px - 1, py - 1, pw + 2, ph + 2, rad(9), p.border, 0.9);
    let panel_a = if st.look == Look::Win11 { 0.92 } else { 0.97 };
    cv.round_rect_a(px, py, pw, ph, rad(8), p.panel, panel_a);

    let inner = pw - PAD * 2;
    let foot_y = py + ph - FOOT_H;

    // ── the header: [chip] title  path ──
    let head_y = py + PAD;
    let mut hx = px + PAD;
    if let Some(chip) = mode_chip(st) {
        let cw = Canvas::text_width(chip) + 12;
        if cw * 3 < inner {
            cv.round_rect(hx, head_y - 1, cw, GLYPH_H + 6, rad(4), p.accent);
            cv.text_bold(chip, hx + 6, head_y + 2, p.on_accent);
            hx += cw + 10;
        }
    }
    // The title can come from an application through the portal, so its length
    // is not ours to trust: the header shares its line with the path, and a
    // 500-character title would leave the path nowhere to go (`path_room` has a
    // floor, so what gets pushed off the panel is the path).
    let mut title = files::elide_middle(
        st.title.as_deref().unwrap_or(match st.mode {
            Mode::Save => st.lang.s(Key::file_title_save),
            // The program's own name, which is not translated.
            Mode::Browse if hud => "Lunarfile",
            Mode::Browse => st.lang.s(Key::file_title_browse),
            _ => st.lang.s(Key::file_title_pick),
        }),
        MAX_TITLE,
    );
    let title_c = if hud {
        // `■ LUNARFILE · /path`: the window's marker, its name, its place.
        cv.fill_rect_a(hx, head_y + 6, 6, 6, p.accent, 1.0);
        hx += 14;
        title = format!("{} ·", title.to_uppercase());
        p.dim
    } else if st.mode == Mode::Browse {
        p.accent
    } else {
        p.text
    };
    cv.text_bold(&title, hx, head_y + 2, title_c);
    let title_w = Canvas::text_width(&title) + if hud { GLYPH_W } else { COL_GAP };
    let path_room = ((px + PAD + inner - hx - title_w) / GLYPH_W).max(8) as usize;
    cv.text(
        &files::elide_middle(&st.dir, path_room),
        hx + title_w,
        head_y + 2,
        if hud { p.text } else { p.dim },
    );
    cv.hline(px + PAD, head_y + HEAD_H - 6, inner, p.border, 0.8);

    // ── the places rail ──
    let body_y = head_y + HEAD_H;
    let rail = rail_w(pw);
    let mut places = Vec::new();
    if rail > 0 {
        let rx = px + PAD;
        let rw = rail - 12;
        cv.text(&cased(st, Key::file_places), rx + 8, body_y + 2, p.dim);
        let mut y = body_y + GLYPH_H + 10;
        let chars = ((rw - 16 - 14 - 8) / GLYPH_W).max(1) as usize;
        for pl in &st.places {
            // Only what fits is drawn, and only what is drawn is clickable.
            if y + PLACE_H > foot_y - 4 {
                break;
            }
            let here = pl.path == st.dir;
            if here {
                cv.round_rect_a(rx, y, rw, PLACE_H - 2, rad(5), p.sel, 0.95);
                if hud {
                    cv.fill_rect_a(rx, y, 2, PLACE_H - 2, p.accent, 1.0);
                }
            }
            let (ic, tc) = if here {
                (p.sel_text, p.sel_text)
            } else {
                (p.accent, p.text)
            };
            // Tecno lists places as words, the way the mockup does.
            if !hud {
                folder_icon(cv, rx + 8, y + (PLACE_H - 2 - 14) / 2, 14, ic);
            }
            let ty = y + (PLACE_H - 2 - GLYPH_H) / 2;
            let tx = if hud { rx + 12 } else { rx + 30 };
            cv.text(&files::elide_middle(&pl.label, chars), tx, ty, tc);
            places.push((rx, y, rw, PLACE_H - 2));
            y += PLACE_H;
        }
        cv.fill_rect_a(
            px + PAD + rail - 6,
            body_y,
            1,
            foot_y - body_y - 6,
            p.border,
            0.8,
        );
    }

    // ── the list column: field, column titles, rows ──
    let lx = px + PAD + rail;
    let prev = preview_w(pw);
    let lw = inner - rail - prev;
    if prev > 0 {
        let vx = lx + lw + 12;
        cv.fill_rect_a(vx - 6, body_y, 1, foot_y - body_y - 6, p.border, 0.8);
        draw_preview(cv, st, vx, body_y, prev - 12, foot_y - body_y - 8);
    }

    let field_y = body_y;
    let field_c = if st.armed() {
        WARN
    } else if st.edit.is_some() {
        p.accent
    } else {
        p.border
    };
    cv.round_rect(lx, field_y, lw, FIELD_BOX, rad(6), field_c);
    cv.round_rect(lx + 1, field_y + 1, lw - 2, FIELD_H - 8, rad(5), p.field);
    let label = st.lang.s(if let Some(e) = &st.edit {
        match e {
            Edit::Rename(_) => Key::file_label_rename,
            Edit::NewDir => Key::file_label_newdir,
        }
    } else if st.mode == Mode::Save {
        Key::file_label_name
    } else {
        Key::file_label_filter
    });
    let ty = field_y + (FIELD_BOX - GLYPH_H) / 2;
    cv.text(label, lx + 8, ty, p.dim);
    let lbw = Canvas::text_width(label) + 8;
    // A caret, so an empty field reads as one waiting for typing rather than as
    // a disabled control.
    let shown = files::elide_middle(&st.field, ((lw - lbw - 24) / GLYPH_W).max(4) as usize);
    let tw = cv.text(&shown, lx + 8 + lbw, ty, p.text);
    cv.fill_rect_a(lx + 8 + lbw + tw + 2, ty, 2, GLYPH_H, p.accent, 0.9);

    let check = if st.mode == Mode::OpenMany {
        CHECK_W
    } else {
        0
    };
    let ix = lx + 6 + check;
    let name_x = ix + ICON + 8;
    let avail = lw - (ICON + 14) - check;
    let dates = shows_dates(avail, st.lang);
    let date_x = lx + lw - 8 - date_col(st.lang) * GLYPH_W;
    let size_r = if dates { date_x - COL_GAP } else { lx + lw - 8 };

    let cols = cols_h(ph);
    let row0_y = field_y + FIELD_BOX + PAD + cols;
    if cols > 0 {
        let cy = row0_y - cols;
        let [nom, tam, modi] = [Key::file_col_name, Key::file_col_size, Key::file_col_modified]
            .map(|k| cased(st, k));
        cv.text(&nom, name_x, cy, p.dim);
        cv.text(&tam, size_r - Canvas::text_width(&tam), cy, p.dim);
        if dates {
            cv.text(&modi, date_x, cy, p.dim);
        }
    }

    let now = files::now_secs();
    let offset = files::local_offset(now);
    let fits = rows_fitting(ph);
    let shown = st.rows.len().min(st.top + fits);
    for (n, row) in (st.top..shown).enumerate() {
        let y = row0_y + n as i32 * ROW_H;
        let selected = row == st.sel;
        let hovered = st.hover == Some(row);
        if selected && hud {
            // A's selected row: the plate, framed in a cyan hairline.
            cv.fill_rect_a(lx, y, lw, ROW_H - 2, p.accent, 0.9);
            cv.fill_rect_a(lx + 1, y + 1, lw - 2, ROW_H - 4, p.sel, 1.0);
        } else if selected {
            cv.round_rect_a(lx, y, lw, ROW_H - 2, 5, p.sel, 0.95);
        } else if hovered {
            cv.round_rect_a(lx, y, lw, ROW_H - 2, rad(5), p.field, 0.9);
        }
        let (tc, dc) = if selected {
            (p.sel_text, p.sel_text)
        } else {
            (p.text, p.dim)
        };
        let ty = y + (ROW_H - 2 - GLYPH_H) / 2;
        let iy = y + (ROW_H - 2 - (ICON - 4)) / 2;
        match st.rows[row] {
            Row::Parent => {
                // A left-pointing triangle, which is also what ← does.
                let c = if selected { p.sel_text } else { p.accent };
                cv.triangle_h(ix + 2, y + (ROW_H - ICON) / 2 + 4, ICON - 8, true, c);
                cv.text("..", name_x, ty, tc);
            }
            Row::Entry(i) => {
                let Some(e) = st.entries.get(i) else { continue };
                if check > 0 && !e.is_dir {
                    let marked = st
                        .marked
                        .iter()
                        .any(|m| *m == files::join_child(&st.dir, &e.name));
                    let cy = y + (ROW_H - 2 - 14) / 2;
                    if marked {
                        cv.badge(lx + 6, cy, 14, 'x', p.accent, p.on_accent);
                    } else {
                        cv.round_rect(lx + 6, cy, 14, 14, 3, dc);
                        let inside = if selected { p.sel } else { p.panel };
                        cv.round_rect(lx + 8, cy + 2, 10, 10, 2, inside);
                    }
                }
                if e.is_dir && hud {
                    // `▸ name`, both gold: a folder is somewhere to go.
                    cv.triangle_h(ix + 5, y + (ROW_H - 2 - 8) / 2, 8, false, GOLD);
                } else if e.is_dir {
                    let c = if selected { p.sel_text } else { p.accent };
                    folder_icon(cv, ix + 2, iy, ICON - 4, c);
                } else if hud {
                    // Files carry no glyph in A; the name says enough.
                } else {
                    let c = kind_color(files::kind_of(&e.name), p);
                    file_icon(cv, ix + 2, iy, ICON - 4, c);
                }
                let mut name = files::elide_middle(&e.name, name_chars(avail, dates, st.lang));
                if e.link {
                    name.push('@');
                }
                let tc = if hud && e.is_dir { GOLD } else { tc };
                cv.text(&name, name_x, ty, tc);
                // The size column, right-aligned. A directory has no
                // meaningful one.
                let size = if e.is_dir {
                    String::from("--")
                } else {
                    files::human_size(e.size, st.lang)
                };
                let sw = Canvas::text_width(&size);
                cv.text(&size, size_r - sw, ty, dc);
                if dates {
                    if let Some(t) = e.mtime {
                        cv.text(&files::short_date(t, now, offset, st.lang), date_x, ty, dc);
                    }
                }
            }
        }
    }

    // ── the footer: hints and status on the left, buttons on the right ──
    cv.hline(px + PAD, foot_y, inner, p.border, 0.8);
    let mut buttons = Vec::new();
    let mut text_r = px + PAD + inner;
    if st.mode.is_pick() {
        let primary = primary_label(st);
        let pw_ = Canvas::text_width(&primary) + 28;
        let cancel = st.lang.s(Key::file_btn_cancel);
        let cw = Canvas::text_width(cancel) + 28;
        let by = foot_y + (FOOT_H - BTN_H) / 2 + 1;
        let bx = px + PAD + inner - pw_;
        let cx = bx - 8 - cw;
        // Only when the hints keep room to be read beside them.
        if cx - (px + PAD) >= 24 * GLYPH_W {
            let (fill, on) = if st.armed() {
                (WARN, ON_WARN)
            } else {
                (p.accent, p.on_accent)
            };
            let bty = by + (BTN_H - GLYPH_H) / 2;
            cv.round_rect(bx, by, pw_, BTN_H, rad(6), fill);
            cv.text_bold(&primary, bx + 14, bty, on);
            cv.round_rect(cx, by, cw, BTN_H, rad(6), p.border);
            cv.round_rect(cx + 1, by + 1, cw - 2, BTN_H - 2, rad(5), p.panel);
            cv.text(cancel, cx + 14, bty, p.text);
            buttons.push(((cx, by, cw, BTN_H), Button::Cancel));
            buttons.push(((bx, by, pw_, BTN_H), Button::Primary));
            text_r = cx - 12;
        }
    }
    let text_chars = ((text_r - (px + PAD + 4)) / GLYPH_W).max(8) as usize;
    let hint = st.lang.s(match st.mode {
        Mode::Save => Key::file_hint_save,
        Mode::OpenMany => Key::file_hint_many,
        Mode::Open => Key::file_hint_open,
        Mode::Browse if st.edit.is_some() => Key::file_hint_edit,
        Mode::Browse => Key::file_hint_browse,
    });
    cv.text(
        &files::elide_middle(hint, text_chars),
        px + PAD + 4,
        foot_y + 4,
        p.dim,
    );
    // The second line is the last thing that went wrong, or the count.
    let status = if !st.note.is_empty() {
        files::elide_middle(&st.note, text_chars)
    } else if st.mode == Mode::OpenMany && !st.marked.is_empty() {
        format!("{} {}", st.marked.len(), st.lang.s(Key::file_marked))
    } else if let Some((h, cut)) = &st.held {
        let n = h.rsplit('/').next().unwrap_or(h);
        format!(
            "{}: {n} · {}",
            st.lang.s(if *cut { Key::file_cut } else { Key::file_copied }),
            st.lang.s(Key::file_paste_here)
        )
    } else {
        let files_n = st.rows.iter().filter(|r| !matches!(r, Row::Parent)).count();
        format!("{files_n} {}", st.lang.s(Key::file_items))
    };
    let c = if st.armed() {
        WARN
    } else if st.note.is_empty() {
        p.dim
    } else {
        p.accent
    };
    let sy = foot_y + 4 + GLYPH_H + 2;
    if let Some(j) = &st.job {
        // A paste in progress: its bar, then how far and how to stop it.
        let bw = job::draw_bar(cv, st, j, px + PAD + 4, sy + 2);
        let room = ((text_r - (px + PAD + 4) - bw - 12) / GLYPH_W).max(8) as usize;
        cv.text(&files::elide_middle(&j.line(st.lang), room), px + PAD + 4 + bw + 12, sy, p.text);
    } else {
        cv.text(&status, px + PAD + 4, sy, c);
    }

    // The action menu, over everything, under the selected row.
    let sel_y = row0_y + (st.sel.saturating_sub(st.top)) as i32 * ROW_H + ROW_H - 2;
    let menu = actions::draw_menu(cv, st, lx, lw, sel_y, foot_y);

    Frame {
        panel: (px, py, pw, ph),
        list: (lx, py, lw, ph),
        row0_y,
        rows_shown: fits,
        places,
        buttons,
        menu,
    }
}

// ── wayland plumbing ─────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    st.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => st.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    st.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_output" => {
                    if st.outputs.len() < fill_guard::MAX_OUTPUTS {
                        st.outputs.push(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "wl_seat" if st.seat.is_none() => {
                    st.seat = Some(registry.bind(name, version.min(5), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        st: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                st.configure(qh, width, height);
            }
            // The compositor took the surface away: nothing left to run in. A
            // `--pick` that has not answered yet exits having answered nothing,
            // which the caller reads as a cancel -- the right outcome, since
            // nobody chose anything.
            zwlr_layer_surface_v1::Event::Closed => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (usize, u64)> for State {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &(i, generation): &(usize, u64),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            let _ = st.buffer_released(i, generation);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && st.keyboard.is_none() {
                st.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && st.pointer.is_none() {
                st.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        else {
            return;
        };
        apply(st, key_action(key, st.mode));
    }
}

/// What a key does once it has been named.
///
/// Split out of the keyboard handler so that the tests can press keys without
/// a compositor to deliver them: everything here goes through `State`, and
/// `render` is a no-op until a surface is configured.
fn apply(st: &mut State, action: Action) {
    // The action menu, while it is open, takes the keys.
    if let Some(m) = st.menu {
        let n = actions::OPS.len() as i32;
        match action {
            Action::Close | Action::Menu => st.menu = None,
            Action::Move(d, _) => st.menu = Some((m as i32 + d).rem_euclid(n) as usize),
            Action::Activate | Action::Commit => st.op(actions::OPS[m]),
            Action::Op(o) => st.op(o),
            _ => {}
        }
        st.render();
        return;
    }
    // So does a name being typed: Enter does it, Esc drops it, and the keys
    // that would move or act elsewhere do nothing until then.
    if st.edit.is_some() {
        match action {
            Action::Close => st.cancel_edit(),
            Action::Activate | Action::Commit => st.finish_edit(),
            Action::Rub => {
                st.field.pop();
            }
            Action::Type(c) if st.field.chars().count() < MAX_FIELD => st.field.push(c),
            _ => {}
        }
        st.render();
        return;
    }
    // Esc stops a running paste before it closes anything.
    if action == Action::Close && st.cancel_job() {
        st.render();
        return;
    }
    match action {
        Action::Op(o) => {
            st.op(o);
            st.render();
        }
        Action::Menu => {
            st.menu = Some(0);
            st.render();
        }
        Action::Close => st.done = true,
        Action::Activate => {
            let sel = st.sel;
            st.activate(sel);
            st.render();
        }
        Action::Commit => {
            st.commit();
            st.render();
        }
        Action::Up => {
            st.go_up();
            st.render();
        }
        Action::Rub => {
            // A character off the field, or -- with nothing left to rub
            // out -- up one directory, which is what Backspace means in
            // every file browser.
            if st.field.is_empty() {
                st.go_up();
            } else {
                st.field.pop();
                st.confirm = None;
                st.refilter();
            }
            st.render();
        }
        Action::Move(delta, step) => {
            st.move_sel(delta, step);
            st.render();
        }
        Action::Page(pages) => {
            // One page is what the last frame drew, not what the panel can
            // hold at most.
            let delta = pages * st.rows_shown.max(1) as i32;
            st.move_sel(delta, Step::Clamp);
            st.render();
        }
        Action::Mark => {
            let sel = st.sel;
            st.toggle_mark(sel);
            st.render();
        }
        Action::Type(c) => {
            if st.field.chars().count() < MAX_FIELD {
                st.field.push(c);
                st.note.clear();
                st.confirm = None;
                st.refilter();
                st.render();
            }
        }
        Action::Ignore => {}
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                st.ptr = (surface_x, surface_y);
                if st.menu.is_some() {
                    if let Some(i) = st.menu_at(surface_x, surface_y) {
                        if st.menu != Some(i) {
                            st.menu = Some(i);
                            st.render();
                        }
                    }
                    return;
                }
                let row = st.row_at(surface_x, surface_y);
                if row != st.hover {
                    st.hover = row;
                    st.render();
                }
            }
            wl_pointer::Event::Leave { .. } => {
                st.ptr = (-1.0, -1.0);
                if st.hover.is_some() {
                    st.hover = None;
                    st.render();
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                let (x, y) = st.ptr;
                if st.menu.is_some() {
                    // A click on an action does it; anywhere else closes the menu.
                    match st.menu_at(x, y) {
                        Some(i) if button == BTN_LEFT => st.op(st.menu_rects[i].1),
                        _ => st.menu = None,
                    }
                    st.render();
                    return;
                }
                if button == BTN_RIGHT && st.mode == Mode::Browse && st.edit.is_none() {
                    if let Some(row) = st.row_at(x, y) {
                        st.sel = row;
                        st.menu = Some(0);
                        st.render();
                    }
                    return;
                }
                if button != BTN_LEFT {
                    return;
                }
                if let Some(b) = st.button_at(x, y) {
                    st.press(b);
                    st.render();
                } else if let Some(i) = st.place_at(x, y) {
                    st.go_place(i);
                    st.render();
                } else if let Some(row) = st.row_at(x, y) {
                    // A click selects AND activates: one click is what a user
                    // does to a row in a chooser, and a double-click protocol
                    // needs a clock this client does not keep.
                    st.sel = row;
                    st.activate(row);
                    st.render();
                } else if !st.inside_panel(x, y) {
                    st.done = true;
                }
            }
            wl_pointer::Event::Axis { value, .. } => {
                let (top, acc) = scroll_top(
                    st.top,
                    st.scroll_acc,
                    value,
                    st.rows.len(),
                    st.rows_shown,
                    WHEEL_NOTCH,
                );
                st.scroll_acc = acc;
                if top != st.top {
                    st.top = top;
                    let (x, y) = st.ptr;
                    st.hover = row_at_geom(
                        st.list_rect,
                        st.row0_y,
                        ROW_H,
                        st.rows_shown,
                        top,
                        st.rows.len(),
                        x,
                        y,
                    );
                    st.render();
                }
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

/// Floors for `--dump`, so a tiny request still renders something a person can
/// look at. The ceilings are `fill_guard::check_buffer`'s.
const MIN_DUMP_W: usize = 320;
const MIN_DUMP_H: usize = 240;

/// Split `--dump PATH:WxH` into its three parts, defaulting the size.
///
/// The size is the last `:` segment ONLY when it is digits, `x`, digits —
/// `lunarrun` learnt this the hard way: accepting any segment that merely
/// contained an `x` and then falling back to a default on a failed parse made
/// `--dump /tmp/frame:extra.raw` write `/tmp/frame` at the default size, losing
/// part of the path AND the requested size without a word. `parse` alone is not
/// the check either, because `usize::from_str` accepts a leading `+`.
fn parse_dump_spec(spec: &str) -> (String, usize, usize) {
    let dims = spec.rsplit_once(':').and_then(|(path, dims)| {
        let (ws, hs) = dims.split_once('x')?;
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(ws) || !digits(hs) {
            return None;
        }
        Some((path.to_string(), ws.parse().ok()?, hs.parse().ok()?))
    });
    dims.unwrap_or((spec.to_string(), 1280, 720))
}

/// Everything `--dump PATH:WxH` decides: the path, the size, the floors and the
/// ceilings — the same ceilings `configure` applies, in one place, so the
/// offline render cannot stand in for a surface it would not have fitted.
fn dump_target(spec: &str) -> Result<(String, usize, usize), fill_guard::TooBig> {
    let (path, w, h) = parse_dump_spec(spec);
    let (w, h) = (w.max(MIN_DUMP_W), h.max(MIN_DUMP_H));
    fill_guard::check_buffer(w, h)?;
    Ok((path, w, h))
}

/// The buffer size for a `configure` of `w` x `h`, or None for something this
/// client will not render. A zero means "you choose".
fn surface_size(w: u32, h: u32) -> Option<(u32, u32)> {
    let w = if w == 0 { 1280 } else { w };
    let h = if h == 0 { 720 } else { h };
    fill_guard::check_buffer(w as usize, h as usize).ok()?;
    Some((w, h))
}

// ── the command line ─────────────────────────────────────────────────────────

/// What the command line asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Cli {
    mode: Option<Mode>,
    /// Where to start. A bare word is taken as this, so `lunarfile /etc` works.
    dir: Option<String>,
    /// `--save NAME`: the name the field starts with.
    name: Option<String>,
    out: Option<String>,
    /// `--title TEXT`: what the header says, instead of the mode's own word.
    /// The portal passes the title the application wrote, already translated,
    /// so the dialog says what Firefox asked for rather than "Elegir archivo".
    title: Option<String>,
    hidden: bool,
    dump: Option<String>,
}

impl Cli {
    /// The mode the flags add up to. `--multiple` and `--save` imply `--pick`,
    /// because a caller that asks for either is asking for a chooser.
    fn mode(&self) -> Mode {
        self.mode.unwrap_or(Mode::Browse)
    }
}

/// Parse the command line. `Err` carries what was wrong, for the usage text.
///
/// Taking an iterator rather than reading `env::args` inside: this is the whole
/// of the argument handling, and a function that reads the process's own
/// arguments can only be tested by running the process.
fn parse_args_from(args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut multiple = false;
    let mut pick = false;
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pick" => pick = true,
            "--multiple" => multiple = true,
            "--hidden" => cli.hidden = true,
            // Every flag that takes a value refuses to swallow the next FLAG as
            // its value: `--out --hidden` is a forgotten filename, and writing
            // the answer to a file called `--hidden` helps nobody.
            "--save" => {
                cli.name = Some(
                    args.next_if(|n| !n.starts_with('-'))
                        .ok_or("--save needs a NAME")?,
                )
            }
            "--dir" => {
                cli.dir = Some(
                    args.next_if(|n| !n.starts_with('-'))
                        .ok_or("--dir needs a DIR")?,
                )
            }
            "--title" => {
                cli.title = Some(
                    args.next_if(|n| !n.starts_with('-'))
                        .ok_or("--title needs TEXT")?,
                )
            }
            "--out" => {
                cli.out = Some(
                    args.next_if(|n| !n.starts_with('-'))
                        .ok_or("--out needs a FILE")?,
                )
            }
            "--dump" => {
                cli.dump = Some(
                    args.next_if(|n| !n.starts_with('-'))
                        .ok_or("--dump needs PATH:WxH")?,
                )
            }
            "-h" | "--help" => return Err("help".into()),
            // A bare word is the directory to open in, so `eclipse-files /etc`
            // and the labwc menu both work. A second one is a mistake worth
            // saying out loud rather than silently keeping the last.
            other if !other.starts_with('-') => {
                if cli.dir.is_some() {
                    return Err(format!("two directories given ({other})"));
                }
                cli.dir = Some(other.to_string());
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    // `--save` and `--multiple` are both choosers, and they are not the same
    // chooser: a Save that also multi-selected would have two answers and one
    // filename field.
    if cli.name.is_some() && multiple {
        return Err("--save and --multiple are different choosers".into());
    }
    cli.mode = Some(
        match (pick || cli.name.is_some() || multiple, multiple, &cli.name) {
            (false, _, _) => Mode::Browse,
            (true, _, Some(_)) => Mode::Save,
            (true, true, None) => Mode::OpenMany,
            (true, false, None) => Mode::Open,
        },
    );
    Ok(cli)
}

fn usage() -> ! {
    eprintln!(
        "usage: lunarfile [DIR] [--pick [--multiple | --save NAME]] [--out FILE]\n\
         \x20              [--title TEXT] [--hidden] [--dump PATH:WxH]\n\
         \n\
         \x20 (no args)      browse $HOME; Enter opens a file with its handler\n\
         \x20 --pick         choose a file: its path is printed on Enter\n\
         \x20 --multiple     choose several; Space marks, one path per line\n\
         \x20 --save NAME    choose where to save; prints <dir>/<name>\n\
         \x20 --title TEXT   the header, instead of the mode's own word\n\
         \x20 --out FILE     write the answer to FILE instead of stdout\n\
         \x20 --dir DIR      start here instead of $HOME\n\
         \x20 --hidden       show dotfiles (a filter starting with `.` also does)\n\
         \x20 --dump P:WxH   render the browser to a raw ARGB8888 file\n\
         \n\
         exit: 0 a file was chosen, 1 cancelled or failed\n\
         env: ECLIPSE_LOOK=kde|eclipse|win11"
    );
    std::process::exit(2)
}

fn new_state(cli: &Cli) -> State {
    // ONE read of the look, not two: `eclipse-look` rewrites /etc/eclipse/look
    // when the user switches, and two calls could leave the palette from one
    // look beside the geometry of the other.
    let look = Look::current();
    let mode = cli.mode();
    let mut st = State {
        compositor: None,
        shm: None,
        layer_shell: None,
        seat: None,
        keyboard: None,
        pointer: None,
        outputs: Vec::new(),
        surface: None,
        layer: None,
        width: 0,
        height: 0,
        map: std::ptr::null_mut(),
        map_len: 0,
        buffers: [None, None],
        busy: [false, false],
        dirty: false,
        next: 0,
        generation: 0,
        configured: false,
        pal: palette(look),
        look,
        lang: Lang::current(),
        mode,
        out: cli.out.clone(),
        openers: open::load_openers(),
        always_hidden: cli.hidden,
        title: cli.title.clone(),
        dir: String::from("/"),
        entries: Vec::new(),
        note: String::new(),
        field: cli.name.clone().unwrap_or_default(),
        rows: Vec::new(),
        marked: Vec::new(),
        picked: Vec::new(),
        places: files::places(
            std::env::var("HOME").ok().as_deref(),
            std::env::var("USER").ok().as_deref(),
            Lang::current(),
        ),
        confirm: None,
        sel: 0,
        top: 0,
        hover: None,
        ptr: (-1.0, -1.0),
        panel: (0, 0, 0, 0),
        list_rect: (0, 0, 0, 0),
        place_rects: Vec::new(),
        buttons: Vec::new(),
        menu: None,
        menu_rects: Vec::new(),
        edit: None,
        kept_filter: String::new(),
        held: None,
        job: None,
        row0_y: 0,
        rows_shown: MAX_ROWS,
        scroll_acc: 0.0,
        done: false,
        preview: Default::default(),
    };
    let start = match cli.dir.as_deref() {
        Some(d) => files::normalize_dir(d),
        None => files::default_dir(std::env::var("HOME").ok().as_deref()),
    };
    // `chdir` clears the field on a successful move, which would throw away the
    // `--save NAME` just put there, so put it back.
    let name = st.field.clone();
    st.chdir(&start);
    // A `--dir` that does not exist falls back to a directory that does rather
    // than leaving the browser with nothing: the caller's suggestion is not
    // worth failing the whole chooser over.
    if st.entries.is_empty() && !st.note.is_empty() {
        let home = files::default_dir(std::env::var("HOME").ok().as_deref());
        if home != start {
            let first = std::mem::take(&mut st.note);
            st.chdir(&home);
            st.note = first;
        }
    }
    st.field = name;
    st.refilter();
    st
}

/// Write the answer where the caller asked for it: one path per line, with a
/// trailing newline, which is what `read` in a shell and `lines()` in anything
/// else both expect.
fn emit(picked: &[String], out: Option<&str>) -> std::io::Result<()> {
    if picked.is_empty() {
        return Ok(());
    }
    let mut text = picked.join("\n");
    text.push('\n');
    match out {
        Some(path) => std::fs::write(path, text),
        None => {
            use std::io::Write;
            let mut so = std::io::stdout();
            so.write_all(text.as_bytes())?;
            so.flush()
        }
    }
}

fn main() {
    let cli = match parse_args_from(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            if e != "help" {
                eprintln!("lunarfile: {e}");
            }
            usage()
        }
    };

    // Offline render: no compositor, no seat, just a file to eyeball.
    if let Some(spec) = cli.dump.clone() {
        let (path, w, h) = match dump_target(&spec) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("lunarfile: {spec}: past {e:?}; nothing written");
                std::process::exit(1);
            }
        };
        let mut st = new_state(&cli);
        // LUNARFILE_DUMP_SEL=name: the frame with that entry selected.
        if let Ok(name) = std::env::var("LUNARFILE_DUMP_SEL") {
            if let Some(r) = st.rows.iter().position(|r| matches!(r, Row::Entry(i) if st.entries[*i].name == name)) {
                st.sel = r;
            }
        }
        // LUNARFILE_DUMP_MENU=n: with the action menu open on row n.
        st.menu = std::env::var("LUNARFILE_DUMP_MENU").ok().and_then(|v| v.parse().ok());
        // LUNARFILE_DUMP_JOB=0.45: a paste that far along (drawn, not run).
        if let Some(f) = std::env::var("LUNARFILE_DUMP_JOB").ok().and_then(|v| v.parse::<f64>().ok()) {
            let prog = std::sync::Arc::new(lunarbar::fileops::Progress::default());
            prog.total.store(4_300_000_000, std::sync::atomic::Ordering::Relaxed);
            prog.done.store((4_300_000_000.0 * f) as u64, std::sync::atomic::Ordering::Relaxed);
            st.job = Some(job::Job { handle: None, prog, name: "eclipse.iso".into(), cut: false, dir: st.dir.clone() });
        }
        // LUNARFILE_DUMP_KEYS=60: those key codes pressed first (F2 renames...).
        for k in std::env::var("LUNARFILE_DUMP_KEYS").unwrap_or_default().split(',').filter_map(|k| k.parse().ok()) {
            let a = key_action(k, st.mode);
            apply(&mut st, a);
        }
        // try_new, not new: refusing beats aborting (`panic = "abort"`).
        let Some(mut cv) = Canvas::try_new(w, h) else {
            eprintln!("lunarfile: cannot allocate a {w}x{h} canvas");
            std::process::exit(1);
        };
        let _ = draw_browser(&mut cv, w, h, &st);
        let mut buf = vec![0u8; w * h * 4];
        if !cv.blit_argb(&mut buf) {
            eprintln!("lunarfile: blit failed");
            std::process::exit(1);
        }
        if let Err(e) = std::fs::write(&path, &buf) {
            eprintln!("lunarfile: {path}: {e}");
            std::process::exit(1);
        }
        println!(
            "lunarfile: wrote {path} ({w}x{h} ARGB8888, {} in {})",
            st.rows.len(),
            st.dir
        );
        return;
    }

    // Not `connect_to_env`: the portal and labwc's autostart start us
    // with no WAYLAND_DISPLAY. See `lunarbar::wl`.
    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarfile: no Wayland compositor found (is the session up?)");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut st = new_state(&cli);
    // Two roundtrips: the first delivers the globals, the second the events
    // they emit on bind (the seat's capabilities).
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarfile: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    if st.layer_shell.is_none() || st.shm.is_none() || st.compositor.is_none() {
        eprintln!("lunarfile: compositor lacks wlr-layer-shell/wl_shm");
        std::process::exit(1);
    }
    st.map_overlay(&qh);
    // Blocking until the compositor says something, except while a paste
    // runs: then the loop also wakes every tick to redraw its bar.
    let tick = 100;
    while !st.done {
        if queue.flush().is_err() {
            break;
        }
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let timeout = if st.job.is_some() { tick } else { -1 };
            // SAFETY: one valid pollfd for the length of the call.
            if unsafe { libc::poll(&mut pfd, 1, timeout) } > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        if queue.dispatch_pending(&mut st).is_err() {
            break;
        }
        if st.tick_job() {
            st.render();
        }
    }
    let (picked, out, pick) = (
        std::mem::take(&mut st.picked),
        st.out.clone(),
        st.mode.is_pick(),
    );
    // Tear the overlay down BEFORE the answer is written: a caller that is
    // waiting on the `--out` file sees it appear once the chooser is off the
    // screen, not while it still covers the window that asked.
    drop(st);
    let _ = conn.flush();
    if picked.is_empty() {
        // Nothing chosen is a cancel, and the exit code is the only way a
        // caller with no output file can tell that apart from an empty answer.
        // Browsing is not a chooser, though: Esc out of Super+E is success.
        std::process::exit(i32::from(pick));
    }
    if let Err(e) = emit(&picked, out.as_deref()) {
        eprintln!("lunarfile: {e}");
        std::process::exit(1);
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<Cli, String> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    // ── the command line ─────────────────────────────────────────────────────

    #[test]
    fn no_arguments_is_the_browser_and_pick_is_the_chooser() {
        assert_eq!(cli(&[]).unwrap().mode(), Mode::Browse);
        assert_eq!(cli(&["--pick"]).unwrap().mode(), Mode::Open);
        assert_eq!(
            cli(&["--pick", "--multiple"]).unwrap().mode(),
            Mode::OpenMany
        );
        assert_eq!(
            cli(&["--pick", "--save", "a.txt"]).unwrap().mode(),
            Mode::Save
        );
        // `--multiple` and `--save` are choosers on their own: a caller that
        // asks for either and forgets `--pick` means the chooser, not a browser
        // that would then OPEN the file it was asked to choose.
        assert_eq!(cli(&["--multiple"]).unwrap().mode(), Mode::OpenMany);
        assert_eq!(cli(&["--save", "a.txt"]).unwrap().mode(), Mode::Save);
        // Browse is the only mode that launches programs.
        assert!(!Mode::Browse.is_pick());
        for m in [Mode::Open, Mode::OpenMany, Mode::Save] {
            assert!(m.is_pick(), "{m:?}");
        }
    }

    #[test]
    fn a_bare_word_is_the_directory_and_a_second_one_is_an_error() {
        assert_eq!(cli(&["/etc"]).unwrap().dir.as_deref(), Some("/etc"));
        assert_eq!(
            cli(&["--pick", "/etc"]).unwrap().dir.as_deref(),
            Some("/etc")
        );
        assert_eq!(
            cli(&["--dir", "/etc"]).unwrap().dir.as_deref(),
            Some("/etc")
        );
        // Silently keeping the last would open somewhere the caller did not
        // name, and silently keeping the first would ignore what they typed.
        assert!(cli(&["/etc", "/var"]).is_err());
        assert_eq!(cli(&[]).unwrap().dir, None);
    }

    #[test]
    fn a_flag_that_takes_a_value_will_not_swallow_the_next_flag() {
        // `--out --hidden` is a forgotten filename, and writing the chooser's
        // answer to a file called `--hidden` helps nobody. Same for every other
        // flag with a value, which is the bug `lunarrun --dump` had.
        for f in ["--out", "--dir", "--save", "--dump", "--title"] {
            assert!(cli(&[f]).is_err(), "{f} alone");
            assert!(cli(&[f, "--hidden"]).is_err(), "{f} ate a flag");
            // And the flag after it is still parsed.
            let ok = cli(&[f, "valor", "--hidden"]).unwrap();
            assert!(ok.hidden, "{f} swallowed --hidden");
        }
        assert_eq!(
            cli(&["--out", "/tmp/a"]).unwrap().out.as_deref(),
            Some("/tmp/a")
        );
        assert_eq!(
            cli(&["--dump", "/tmp/a:8x8"]).unwrap().dump.as_deref(),
            Some("/tmp/a:8x8")
        );
    }

    #[test]
    fn the_title_is_the_callers_and_it_is_bounded() {
        // The portal passes the title the APPLICATION wrote ("Abrir archivo"
        // from Firefox, already translated), so the dialog says what was asked
        // for rather than the mode's own word.
        assert_eq!(
            cli(&["--pick", "--title", "Abrir archivo"])
                .unwrap()
                .title
                .as_deref(),
            Some("Abrir archivo")
        );
        // It is an option, and asking for a title does not make a chooser.
        assert_eq!(cli(&["--title", "Archivos"]).unwrap().mode(), Mode::Browse);
        assert_eq!(cli(&[]).unwrap().title, None);
        // And it is not ours to trust for length: the header shares its line
        // with the path, which has a floor, so a long title would push the path
        // off the panel rather than wrap.
        let long = "x".repeat(500);
        assert_eq!(
            files::elide_middle(&long, MAX_TITLE).chars().count(),
            MAX_TITLE
        );
        // A title that fits is drawn as it is.
        assert_eq!(
            files::elide_middle("Abrir archivo", MAX_TITLE),
            "Abrir archivo"
        );
    }

    #[test]
    fn two_different_choosers_cannot_be_asked_for_at_once() {
        // A Save that also multi-selected would have two answers and one
        // filename field; the caller has to pick one.
        assert!(cli(&["--pick", "--save", "a", "--multiple"]).is_err());
        assert!(cli(&["--multiple", "--save", "a"]).is_err());
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_taken_for_a_directory() {
        assert!(cli(&["--vaya"]).is_err());
        assert!(cli(&["-x"]).is_err());
        assert_eq!(cli(&["-h"]).unwrap_err(), "help");
        assert_eq!(cli(&["--help"]).unwrap_err(), "help");
        // `--hidden` is a flag with no value and does not eat the next word.
        let c = cli(&["--hidden", "/etc"]).unwrap();
        assert!(c.hidden);
        assert_eq!(c.dir.as_deref(), Some("/etc"));
    }

    // ── the key table ────────────────────────────────────────────────────────

    #[test]
    fn the_navigation_keys_mean_the_same_thing_in_every_mode() {
        for m in [Mode::Browse, Mode::Open, Mode::OpenMany, Mode::Save] {
            assert_eq!(key_action(KEY_ESC_WL, m), Action::Close, "{m:?}");
            // Enter is the one key that is NOT the same in every mode: in Save
            // it commits the name in the field, because something is always
            // selected in a populated directory and an Enter that navigated
            // meant the Save chooser could never answer.
            let enter = if m == Mode::Save {
                Action::Commit
            } else {
                Action::Activate
            };
            assert_eq!(key_action(KEY_ENTER_WL, m), enter, "{m:?}");
            assert_eq!(key_action(KEY_KPENTER_WL, m), enter, "{m:?}");
            // → enters, ← goes up: the pair every file browser has, and in
            // Save it is how you reach the folder you are saving into.
            assert_eq!(key_action(KEY_RIGHT_WL, m), Action::Activate);
            assert_eq!(key_action(KEY_LEFT_WL, m), Action::Up);
            assert_eq!(key_action(KEY_BACKSPACE_WL, m), Action::Rub);
            assert_eq!(key_action(KEY_UP_WL, m), Action::Move(-1, Step::Wrap));
            assert_eq!(key_action(KEY_DOWN_WL, m), Action::Move(1, Step::Wrap));
            assert_eq!(key_action(KEY_TAB_WL, m), Action::Move(1, Step::Wrap));
            // A page is resolved against the rows the last frame actually
            // drew, so the key table only says WHICH WAY. Moving by the
            // maximum jumped fourteen rows in a list showing three.
            assert_eq!(key_action(KEY_PGUP_WL, m), Action::Page(-1), "{m:?}");
            assert_eq!(key_action(KEY_PGDN_WL, m), Action::Page(1), "{m:?}");
            // A letter types in every mode: it is the filter, or the filename.
            assert_eq!(key_action(33, m), Action::Type('f'));
            assert_eq!(key_action(52, m), Action::Type('.'));
            // Modifiers and the unmapped rest do nothing at all.
            assert_eq!(key_action(42, m), Action::Ignore);
            assert_eq!(key_action(29, m), Action::Ignore);
        }
    }

    #[test]
    fn space_marks_only_where_marking_is_what_space_is_for() {
        // Filenames have spaces in them. A space that silently did nothing
        // would be the one key the filter and the Save field refused.
        assert_eq!(key_action(57, Mode::OpenMany), Action::Mark);
        for m in [Mode::Browse, Mode::Open, Mode::Save] {
            assert_eq!(key_action(57, m), Action::Type(' '), "{m:?}");
        }
    }

    // ── the rows ─────────────────────────────────────────────────────────────

    fn dir_e(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: true,
            size: 0,
            link: false,
            mtime: None,
        }
    }
    fn file_e(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size: 7,
            link: false,
            mtime: None,
        }
    }

    #[test]
    fn the_parent_row_is_there_everywhere_but_the_root() {
        let e = vec![dir_e("sub"), file_e("a.txt")];
        assert_eq!(rows_of("/home/m", &e, "", false)[0], Row::Parent);
        // At the root there is nowhere up to go, so no row claims there is.
        let at_root = rows_of("/", &e, "", false);
        assert!(!at_root.contains(&Row::Parent));
        assert_eq!(at_root.len(), 2);
    }

    #[test]
    fn a_filter_that_matches_nothing_still_leaves_a_way_out() {
        // Typing a wrong letter must not leave a panel whose only exit is Esc:
        // `..` survives every filter, so ← / Enter on it still works.
        let e = vec![dir_e("sub"), file_e("a.txt")];
        let rows = rows_of("/home/m", &e, "zzzz", false);
        assert_eq!(rows, vec![Row::Parent]);
        // And at the root an empty result really is empty, which is honest.
        assert!(rows_of("/", &e, "zzzz", false).is_empty());
    }

    #[test]
    fn a_row_indexes_the_unfiltered_listing_so_a_filter_cannot_open_the_wrong_file() {
        // THE bug a file browser has to not have. The rows carry indices into
        // the listing as read, not into the filtered view, so row 1 of a
        // filtered list is the entry that matched and not the entry that
        // happened to be second on disk.
        let e = vec![dir_e("aaa"), file_e("uno.txt"), file_e("dos.txt")];
        let rows = rows_of("/home/m", &e, "dos", false);
        assert_eq!(rows, vec![Row::Parent, Row::Entry(2)]);
        let Row::Entry(i) = rows[1] else { panic!() };
        assert_eq!(e[i].name, "dos.txt");
    }

    #[test]
    fn dotfiles_appear_when_the_filter_starts_with_a_dot_or_hidden_was_asked_for() {
        // There is no modifier to hang a Ctrl+H on (that means libxkbcommon and
        // a keymap fd), so typing the dot is the gesture.
        assert!(!show_hidden("", false));
        assert!(!show_hidden("conf", false));
        assert!(show_hidden(".", false));
        assert!(show_hidden(".bash", false));
        // Leading whitespace does not hide the dot, since the filter is
        // trimmed before it is matched.
        assert!(show_hidden("  .bash", false));
        // `--hidden` is for the whole session, whatever is typed.
        assert!(show_hidden("", true));
        assert!(show_hidden("conf", true));
        // A dot anywhere ELSE is an extension being typed, not a request.
        assert!(!show_hidden("a.txt", false));

        let e = vec![dir_e(".config"), file_e("notas.txt")];
        assert_eq!(rows_of("/", &e, "", false).len(), 1);
        assert_eq!(rows_of("/", &e, "", true).len(), 2);
    }

    // ── geometry ─────────────────────────────────────────────────────────────

    #[test]
    fn the_panel_never_hangs_off_the_side_or_the_top_of_the_output() {
        // Swept rather than spot-checked: `panel_rect` is the only thing
        // between a small or odd output and a panel drawn at a negative x,
        // which on this stack is a client that draws nothing at all.
        for w in [160, 320, 640, 800, 1024, 1280, 1920, 3840] {
            for h in [120, 240, 480, 600, 720, 1080, 2160] {
                for rows in [0usize, 1, 5, MAX_ROWS, 200] {
                    let (x, y, pw, ph) = panel_rect(w, h, rows);
                    assert!(x >= 0 && y >= 0, "{w}x{h} rows {rows} -> {x},{y}");
                    assert!(pw > 0 && ph > 0, "{w}x{h} rows {rows} -> {pw}x{ph}");
                    assert!(x + pw <= w, "{w}x{h} rows {rows}: {x}+{pw} past {w}");
                    assert!(y + ph <= h, "{w}x{h} rows {rows}: {y}+{ph} past {h}");
                }
            }
        }
        // Centred, which the launcher's upper-third anchor is not: a file
        // browser is where the attention is for as long as it is open.
        let (x, y, pw, ph) = panel_rect(1280, 720, MAX_ROWS);
        assert_eq!(x, (1280 - pw) / 2);
        assert_eq!(y, (720 - ph) / 2);
    }

    #[test]
    fn the_panel_grows_one_row_at_a_time_up_to_the_row_cap() {
        // On a panel too narrow for the preview column, from a single row.
        let narrow = |rows| panel_rect(960, 1080, rows).3;
        assert_eq!(preview_w(panel_rect(960, 1080, 1).2), 0);
        assert_eq!(narrow(2), narrow(1) + ROW_H);
        assert_eq!(narrow(5), narrow(1) + 4 * ROW_H);
        // With the preview, from the rows it needs to show a picture or a
        // text, however few the folder holds.
        let tall = |rows| panel_rect(1920, 1080, rows).3;
        assert!(preview_w(panel_rect(1920, 1080, 1).2) > 0);
        let one = tall(1);
        assert_eq!(tall(PREVIEW_ROWS), one);
        assert_eq!(tall(PREVIEW_ROWS + 2), one + 2 * ROW_H);
        // Past the cap it stops growing and the list scrolls instead.
        assert_eq!(tall(MAX_ROWS), tall(MAX_ROWS + 1));
        assert_eq!(tall(MAX_ROWS), tall(5000));
        // An empty directory still has a row of height for the "nothing here"
        // space, rather than collapsing to a sliver.
        assert_eq!(tall(0), one);
    }

    #[test]
    fn a_short_output_shows_fewer_rows_rather_than_drawing_past_the_panel() {
        // The rows drawn must be the rows that fit: drawing MAX_ROWS into a
        // panel clipped by a 480-line screen would paint over the footer.
        for h in [120, 240, 360, 480, 600, 720, 1080] {
            let (_, _, _, ph) = panel_rect(1280, h, 200);
            let fits = rows_fitting(ph);
            assert!(fits >= 1, "{h} fits no rows at all");
            assert!(fits <= MAX_ROWS, "{h} fits {fits}, past the cap");
            let used =
                PAD + HEAD_H + FIELD_H + PAD + cols_h(ph) + fits as i32 * ROW_H + FOOT_H + PAD;
            assert!(used <= ph.max(used), "{h}: {used} drawn into {ph}");
        }
        // A roomy panel fits the cap exactly.
        let (_, _, _, ph) = panel_rect(1920, 1080, MAX_ROWS);
        assert_eq!(rows_fitting(ph), MAX_ROWS);
    }

    /// Each column heading fits the column it sits over, in both languages
    /// and in both cases.
    ///
    /// This is the failure a translation makes in a client that draws its own
    /// text: the heading is the ONE string in the row that nothing elides,
    /// because it is written once per frame and not per entry, so a word
    /// wider than its column is simply drawn over the heading beside it. The
    /// Spanish headings were measured against these columns when they were
    /// written; nothing had measured a second language's.
    #[test]
    fn every_column_heading_fits_its_column_in_both_languages() {
        for lang in [Lang::Es, Lang::En] {
            for caps in [false, true] {
                let w = |k: Key| {
                    let s = lang.s(k);
                    let s = if caps { s.to_uppercase() } else { s.to_string() };
                    Canvas::text_width(&s)
                };
                // The name heading is over the name column, whose width is
                // whatever is left; `NAME_MIN` is the least it can be.
                assert!(w(Key::file_col_name) <= NAME_MIN * GLYPH_W, "{lang:?} {caps}");
                assert!(w(Key::file_col_size) <= SIZE_COL * GLYPH_W, "{lang:?} {caps}");
                assert!(
                    w(Key::file_col_modified) <= date_col(lang) * GLYPH_W,
                    "{lang:?} {caps}: the date heading is wider than its column"
                );
                // And the two chips, which sit in a box drawn to their own
                // width but are only drawn where three of them fit the panel.
                for k in [Key::file_chip_pick, Key::file_chip_save] {
                    assert!((w(k) + 12) * 3 <= PANEL_W, "{lang:?} {k:?}");
                }
            }
        }
    }

    #[test]
    fn a_name_is_never_charged_for_a_size_column_that_is_not_drawn() {
        // The same trap `lunarrun`'s row layout had: charging the name for a
        // column the row is too narrow to draw leaves it one character.
        // Both languages: the English date column is four characters wider,
        // and the name is charged for whichever one the session draws.
        for lang in [Lang::Es, Lang::En] {
            assert!(name_chars(800, false, lang) > name_chars(400, false, lang));
            for avail in [0, 1, 20, 60, 100, 200, 400, 800, 4000] {
                for dates in [false, true] {
                    let n = name_chars(avail, dates, lang);
                    assert!(n >= 1, "{avail} px left no room for a single character");
                }
            }
            // With room for both, the name gets what is left after the size
            // column.
            let wide = name_chars(800, false, lang);
            assert!(
                (wide as i32) * GLYPH_W <= 800 - SIZE_COL * GLYPH_W - COL_GAP + GLYPH_W,
                "the name overlaps the size column"
            );
            // And after the date column as well when that is drawn.
            let dated = name_chars(800, true, lang);
            assert!(
                (dated as i32) * GLYPH_W
                    <= 800 - (SIZE_COL + date_col(lang)) * GLYPH_W - 2 * COL_GAP + GLYPH_W,
                "the name overlaps the date column"
            );
            // The date column is only drawn where the name keeps its minimum.
            assert!(!shows_dates(300, lang));
            assert!(shows_dates(700, lang));
            for avail in [0, 100, 300, 400, 500, 600, 700, 800] {
                if shows_dates(avail, lang) {
                    assert!(name_chars(avail, true, lang) as i32 >= NAME_MIN, "{avail}");
                }
            }
        }
    }

    // ── the --dump spec ──────────────────────────────────────────────────────

    #[test]
    fn a_path_that_merely_contains_an_x_is_not_a_size() {
        // The regression `lunarrun` had: the last `:` segment was taken as a
        // size as soon as it held an `x`, then each side fell back to a default
        // when it failed to parse, so `--dump /tmp/frame:extra.raw` wrote
        // `/tmp/frame` at 1280x720, losing the rest of the path without a word.
        assert_eq!(
            parse_dump_spec("/tmp/frame:extra.raw"),
            ("/tmp/frame:extra.raw".to_string(), 1280, 720)
        );
        assert_eq!(
            parse_dump_spec("/tmp/f.raw"),
            ("/tmp/f.raw".to_string(), 1280, 720)
        );
        // `usize::from_str` accepts a leading `+`, so the digit check is not
        // the parse.
        assert_eq!(
            parse_dump_spec("out:+800x600"),
            ("out:+800x600".to_string(), 1280, 720)
        );
        assert_eq!(
            parse_dump_spec("out:x600"),
            ("out:x600".to_string(), 1280, 720)
        );
        assert_eq!(
            parse_dump_spec("out:800x"),
            ("out:800x".to_string(), 1280, 720)
        );
        // A real size is taken, and a path with its own colons survives.
        assert_eq!(
            parse_dump_spec("/tmp/a:b:640x480"),
            ("/tmp/a:b".to_string(), 640, 480)
        );
    }

    #[test]
    fn the_dump_size_lands_inside_the_same_ceiling_the_surface_uses() {
        // One function for both, so the offline render cannot stand in for a
        // surface it would not have fitted.
        let (_, w, h) = dump_target("/tmp/a:1x1").unwrap();
        assert_eq!((w, h), (MIN_DUMP_W, MIN_DUMP_H), "the floors apply");
        assert!(
            dump_target("/tmp/a:16384x16384").is_err(),
            "past the ceiling"
        );
        assert!(dump_target("/tmp/a:1280x720").is_ok());
        // And `surface_size` applies the same one, with a zero meaning
        // "you choose" rather than a 0-byte pool.
        assert_eq!(surface_size(0, 0), Some((1280, 720)));
        assert_eq!(surface_size(1920, 1080), Some((1920, 1080)));
        assert_eq!(surface_size(99999, 99999), None);
    }

    // ── the answer ───────────────────────────────────────────────────────────

    #[test]
    fn the_answer_is_one_path_per_line_with_a_trailing_newline() {
        // `read` in a shell and `lines()` in anything else both want the
        // trailing newline; without it the last path is a partial line.
        let tmp = std::env::temp_dir().join(format!("lunarfile-out-{}", std::process::id()));
        let path = tmp.to_str().unwrap().to_string();
        emit(&["/home/m/a.txt".into()], Some(&path)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "/home/m/a.txt\n");
        emit(
            &["/a".into(), "/b con espacio".into(), "/c'd".into()],
            Some(&path),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "/a\n/b con espacio\n/c'd\n"
        );
        // Nothing chosen writes nothing at all: an empty file would read as a
        // chooser that answered with the empty path.
        std::fs::write(&path, "previo").unwrap();
        emit(&[], Some(&path)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "previo");
        let _ = std::fs::remove_file(&path);
    }

    // ── the browser's state ──────────────────────────────────────────────────

    /// A State with no Wayland objects at all: every one of them is an Option
    /// and nothing below the drawing touches them, so the whole navigation can
    /// be driven without a compositor.
    fn state(mode: Mode, dir: &str) -> State {
        let c = Cli {
            mode: Some(mode),
            dir: Some(dir.to_string()),
            ..Cli::default()
        };
        new_state(&c)
    }

    /// The row a name is on, which is not its index in `entries`: directories
    /// sort first and `..` takes row 0.
    fn row_of(st: &State, name: &str) -> usize {
        (0..st.rows.len())
            .find(|r| st.entry(*r).map(|e| e.name == name).unwrap_or(false))
            .unwrap()
    }

    /// A little tree to walk: `<tmp>/base/{sub/{hondo/},a.txt,b.wav,.oculto}`.
    fn tree(tag: &str) -> String {
        let base = std::env::temp_dir().join(format!("lunarfile-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub/hondo")).unwrap();
        std::fs::write(base.join("a.txt"), b"hola").unwrap();
        std::fs::write(base.join("b.wav"), vec![0u8; 2048]).unwrap();
        std::fs::write(base.join(".oculto"), b"x").unwrap();
        base.to_str().unwrap().to_string()
    }

    // ── acting on files ──────────────────────────────────────────────────────

    fn press(st: &mut State, key: u32) {
        apply(st, key_action(key, st.mode));
    }

    /// Wait for a background paste to end, and take its result.
    fn settle(st: &mut State) {
        while st.job.as_ref().is_some_and(|j| j.running()) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        st.tick_job();
    }

    fn typed(st: &mut State, s: &str) {
        for c in s.chars() {
            apply(st, Action::Type(c));
        }
    }

    #[test]
    fn f2_renames_in_place_and_esc_puts_the_filter_back() {
        let base = tree("rename");
        let mut st = state(Mode::Browse, &base);
        st.sel = row_of(&st, "a.txt");
        press(&mut st, 60); // F2
        assert_eq!(st.field, "a.txt", "the field starts with the old name");
        assert_eq!(st.rows.len(), 4, "the list is not filtered by the name being typed");
        for _ in 0..3 {
            press(&mut st, KEY_BACKSPACE_WL);
        }
        typed(&mut st, "md");
        press(&mut st, KEY_ENTER_WL);
        assert!(std::path::Path::new(&base).join("a.md").exists());
        assert!(st.edit.is_none() && st.field.is_empty());
        assert_eq!(st.entry(st.sel).unwrap().name, "a.md", "the selection follows the file");
        // Esc while renaming leaves everything as it was.
        typed(&mut st, "b");
        st.sel = row_of(&st, "b.wav");
        press(&mut st, 60);
        press(&mut st, KEY_ESC_WL);
        assert_eq!((st.field.as_str(), st.done), ("b", false), "Esc closed the rename, not the browser");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_then_paste_elsewhere_and_a_new_folder_from_the_keys() {
        let base = tree("paste");
        let mut st = state(Mode::Browse, &base);
        press(&mut st, KEY_INSERT_WL);
        assert_eq!(st.note, st.lang.s(Key::file_nothing_to_paste), "{}", st.note);
        st.sel = row_of(&st, "a.txt");
        press(&mut st, 63); // F5
        st.sel = row_of(&st, "sub");
        st.activate(st.sel);
        press(&mut st, KEY_INSERT_WL);
        settle(&mut st);
        let sub = std::path::Path::new(&base).join("sub");
        assert!(sub.join("a.txt").exists() && std::path::Path::new(&base).join("a.txt").exists());
        // Pasting again does not replace, it copies alongside.
        press(&mut st, KEY_INSERT_WL);
        settle(&mut st);
        assert!(sub.join("a (copia).txt").exists());
        press(&mut st, 65); // F7
        // The proposed name is in the session's language, and it is also the
        // name the folder ends up with on disk.
        let new_dir = st.lang.s(Key::file_new_folder_name).to_string();
        assert_eq!(st.field, new_dir);
        press(&mut st, KEY_ENTER_WL);
        assert!(sub.join(&new_dir).is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_menu_is_driven_like_the_list_and_only_the_browser_has_it() {
        let base = tree("menu");
        let mut st = state(Mode::Browse, &base);
        st.sel = row_of(&st, "a.txt");
        press(&mut st, 127); // Menu
        assert_eq!(st.menu, Some(0));
        press(&mut st, KEY_UP_WL);
        assert_eq!(st.menu, Some(actions::OPS.len() - 1), "it wraps");
        press(&mut st, KEY_DOWN_WL);
        press(&mut st, KEY_DOWN_WL); // Renombrar
        press(&mut st, KEY_ENTER_WL);
        assert!(st.menu.is_none());
        assert!(matches!(st.edit, Some(Edit::Rename(_))));
        press(&mut st, KEY_ESC_WL);
        // Drawn, the menu hands back a rect per action.
        st.menu = Some(0);
        let mut cv = Canvas::try_new(1280, 720).unwrap();
        let f = draw_browser(&mut cv, 1280, 720, &st);
        assert_eq!(f.menu.len(), actions::OPS.len());
        for m in [Mode::Open, Mode::OpenMany, Mode::Save] {
            assert_eq!(key_action(60, m), Action::Ignore, "{m:?}");
            assert_eq!(key_action(127, m), Action::Ignore, "{m:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn entering_a_directory_and_climbing_back_out_lands_on_the_one_just_left() {
        // Being dropped at row 0 every time you climb out loses the user's
        // place, which in a deep tree is the difference between usable and not.
        let base = tree("climb");
        let mut st = state(Mode::Browse, &base);
        // row 0 is `..`, row 1 is `sub` (directories first).
        assert_eq!(st.row(0), Some(Row::Parent));
        assert_eq!(st.entry(1).unwrap().name, "sub");
        st.sel = 1;
        st.activate(1);
        assert_eq!(st.dir, files::join_child(&base, "sub"));
        assert_eq!(st.sel, 0, "a fresh directory starts at the top");
        st.go_up();
        assert_eq!(st.dir, files::normalize_dir(&base));
        assert_eq!(
            st.entry(st.sel).map(|e| e.name.as_str()),
            Some("sub"),
            "climbing out must land on the directory just left"
        );
        // The `..` row climbs the same way.
        st.activate(0);
        assert_eq!(st.dir, files::parent_of(&base).unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_filter_does_not_survive_a_move_but_a_save_name_does() {
        // The filter describes THIS directory; carrying it into the next one
        // shows a listing filtered by something the user typed about somewhere
        // else. The Save name is the opposite: it is what they are still
        // typing, and clearing it on every move would make it untypeable.
        let base = tree("field");
        let mut st = state(Mode::Browse, &base);
        st.field = "a".into();
        st.refilter();
        st.chdir(&files::join_child(&base, "sub"));
        assert_eq!(st.field, "");

        let mut st = state(Mode::Save, &base);
        st.field = "informe.pdf".into();
        st.chdir(&files::join_child(&base, "sub"));
        assert_eq!(st.field, "informe.pdf");
        // And in Save the field does NOT filter the listing: narrowing by a
        // name that does not exist yet empties the panel by the third letter.
        assert!(
            st.rows.len() > 1,
            "the Save field must not filter the directory"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_unreadable_directory_leaves_the_browser_where_it_was_and_says_so() {
        // Walking into a directory that cannot be read and showing it empty is
        // a file browser that lies about what is on the disk.
        let base = tree("badly");
        let mut st = state(Mode::Browse, &base);
        let before = st.dir.clone();
        let rows = st.rows.len();
        st.chdir(&files::join_child(&base, "no-existe"));
        assert_eq!(st.dir, before, "it moved into a directory it cannot read");
        assert_eq!(st.rows.len(), rows);
        assert!(!st.note.is_empty(), "nothing told the user");
        assert!(st.note.contains("no-existe"), "{}", st.note);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_chooser_answers_with_a_file_and_never_with_a_directory() {
        let base = tree("pick");
        let mut st = state(Mode::Open, &base);
        let row = (0..st.rows.len())
            .find(|r| st.entry(*r).map(|e| e.name == "a.txt").unwrap_or(false))
            .unwrap();
        st.activate(row);
        assert_eq!(st.picked, vec![files::join_child(&base, "a.txt")]);
        assert!(st.done, "answering ends the loop");

        // A directory is entered, not answered with: a caller that asked for a
        // file cannot open one.
        let mut st = state(Mode::Open, &base);
        let d = (0..st.rows.len())
            .find(|r| st.entry(*r).map(|e| e.is_dir).unwrap_or(false))
            .unwrap();
        st.activate(d);
        assert!(st.picked.is_empty(), "it answered with a directory");
        assert!(!st.done);
        assert_eq!(st.dir, files::join_child(&base, "sub"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn marking_takes_files_only_and_enter_falls_back_to_the_selection() {
        let base = tree("many");
        let mut st = state(Mode::OpenMany, &base);
        // A directory in a multi-select is how a chooser ends up answering with
        // something the caller cannot open.
        let d = row_of(&st, "sub");
        st.toggle_mark(d);
        assert!(st.marked.is_empty(), "a directory got marked");
        // `..` is not markable either.
        st.toggle_mark(0);
        assert!(st.marked.is_empty());

        let a = row_of(&st, "a.txt");
        let b = row_of(&st, "b.wav");
        st.toggle_mark(a);
        st.toggle_mark(b);
        assert_eq!(st.marked.len(), 2);
        // Marking twice unmarks, rather than listing the file twice.
        st.toggle_mark(a);
        assert_eq!(st.marked, vec![files::join_child(&base, "b.wav")]);
        st.toggle_mark(a);
        st.activate(a);
        assert_eq!(st.picked.len(), 2, "Enter answers with everything marked");

        // Enter with nothing marked answers with the selected file, which is
        // what a chooser does when one file is all you wanted.
        let mut st = state(Mode::OpenMany, &base);
        let a = row_of(&st, "a.txt");
        st.activate(a);
        assert_eq!(st.picked, vec![files::join_child(&base, "a.txt")]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn saving_answers_with_a_name_that_does_not_exist_yet() {
        // The whole point of a Save chooser: the file is not there to select.
        let base = tree("save");
        let mut st = state(Mode::Save, &base);
        st.field = "nuevo.txt".into();
        st.save_answer();
        assert_eq!(st.picked, vec![files::join_child(&base, "nuevo.txt")]);
        assert!(st.done);

        // An empty name answers with nothing and says so, rather than handing
        // the caller the directory as if it were a file.
        let mut st = state(Mode::Save, &base);
        st.field = "   ".into();
        st.save_answer();
        assert!(st.picked.is_empty());
        assert!(!st.done);
        assert!(!st.note.is_empty());

        // Clicking an existing file puts its name in the field (overwrite that
        // one) rather than answering behind the user's back with a path they
        // only pointed at.
        let mut st = state(Mode::Save, &base);
        let row = (0..st.rows.len())
            .find(|r| st.entry(*r).map(|e| e.name == "a.txt").unwrap_or(false))
            .unwrap();
        st.activate(row);
        assert!(st.picked.is_empty(), "a click answered the chooser");
        assert_eq!(st.field, "a.txt");
        assert!(!st.done);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn enter_in_save_mode_saves_what_was_typed_and_only_navigates_when_nothing_is() {
        // The Save chooser was undriveable: Enter mapped to `activate`, and
        // `activate` only answers when NO row is selected -- which in a
        // directory with files in it never happens, because row 0 is always
        // selected. So a portal's SaveFile could be typed into and never
        // completed. Enter in Save is its own action now.
        let base = tree("commit");
        let mut st = state(Mode::Save, &base);
        st.field = "nuevo.txt".into();
        // Row 0 is `..`, i.e. exactly the selection that used to swallow Enter.
        assert_eq!(st.row(0), Some(Row::Parent));
        st.sel = 0;
        apply(&mut st, key_action(KEY_ENTER_WL, Mode::Save));
        assert!(st.done, "Enter did not complete the save");
        assert_eq!(st.picked, vec![files::join_child(&base, "nuevo.txt")]);

        // And with a name typed it answers in THIS directory whatever row the
        // cursor happens to be on, including a directory.
        let mut st = state(Mode::Save, &base);
        st.field = "nuevo.txt".into();
        st.sel = (0..st.rows.len())
            .find(|r| st.entry(*r).map(|e| e.name == "sub").unwrap_or(false))
            .unwrap();
        apply(&mut st, Action::Commit);
        assert_eq!(st.picked, vec![files::join_child(&base, "nuevo.txt")]);

        // With an empty field there is nothing to save, so Enter means what it
        // means everywhere else: go in. Typing the name afterwards is then one
        // uninterrupted gesture.
        let mut st = state(Mode::Save, &base);
        let sub = (0..st.rows.len())
            .find(|r| st.entry(*r).map(|e| e.name == "sub").unwrap_or(false))
            .unwrap();
        st.sel = sub;
        apply(&mut st, Action::Commit);
        assert!(!st.done, "an empty field answered the chooser");
        assert_eq!(st.dir, files::join_child(&base, "sub"));
        // And only that: moving in must not also run the save and leave the
        // "type a name" warning on a panel the user has just walked into.
        assert!(st.note.is_empty(), "{}", st.note);
        // `..` is the same gesture upwards.
        st.sel = 0;
        apply(&mut st, Action::Commit);
        assert!(!st.done);
        assert_eq!(st.dir, files::normalize_dir(&base));

        // An empty field on a FILE is not a move and not an answer: it says so.
        let mut st = state(Mode::Save, &base);
        st.sel = row_of(&st, "a.txt");
        apply(&mut st, Action::Commit);
        assert!(!st.done);
        assert!(st.picked.is_empty());
        assert!(!st.note.is_empty(), "nothing told the user");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_page_is_the_rows_the_last_frame_drew_not_the_row_cap() {
        // Paging by `MAX_ROWS` on an output that only fits a handful of rows
        // jumps the cursor far past the screen, so PgDn behaves like End. The
        // page is whatever the frame measured.
        let base = tree("page");
        let mut st = state(Mode::Browse, &base);
        // More rows than any page, so neither direction clamps.
        st.entries = (0..200)
            .map(|n| Entry {
                name: format!("f{n:03}.txt"),
                is_dir: false,
                size: 0,
                link: false,
                mtime: None,
            })
            .collect();
        st.refilter();
        st.sel = 100;
        st.top = 100;

        for shown in [1usize, 7, 23] {
            st.rows_shown = shown;
            st.sel = 100;
            apply(&mut st, Action::Page(1));
            assert_eq!(st.sel, 100 + shown, "PgDn with {shown} rows on screen");
            apply(&mut st, Action::Page(-1));
            assert_eq!(st.sel, 100, "PgUp must undo PgDn");
        }

        // A frame that drew nothing yet must still move by one row rather than
        // standing still: `rows_shown` is 0 before the first frame on a surface
        // too short to draw, and a dead PgDn reads as a broken keyboard.
        st.rows_shown = 0;
        st.sel = 100;
        apply(&mut st, Action::Page(1));
        assert_eq!(st.sel, 101);

        // And paging past the end stops at the end instead of wrapping to the
        // top, which in a 200-row listing is the user losing their place.
        st.rows_shown = 23;
        st.sel = st.rows.len() - 2;
        apply(&mut st, Action::Page(1));
        assert_eq!(st.sel, st.rows.len() - 1);
        st.sel = 1;
        apply(&mut st, Action::Page(-1));
        assert_eq!(st.sel, 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_frame_owed_while_both_buffers_are_held_is_drawn_when_one_comes_back() {
        // `render` wrote `dirty` and nothing ever read it, so a frame dropped
        // because the compositor held both buffers was dropped for good: the
        // panel kept showing the previous rows until the next key happened to
        // land. Here is the pair of decisions, without a compositor.
        assert_eq!(free_buffer([false, false], 0), Some(0));
        assert_eq!(free_buffer([false, false], 1), Some(1));
        // The one just released, not the lowest free one: always picking 0
        // would be single buffering with two allocations.
        assert_eq!(free_buffer([true, false], 0), Some(1));
        assert_eq!(free_buffer([false, true], 1), Some(0));
        assert_eq!(
            free_buffer([true, true], 0),
            None,
            "it drew into a held buffer"
        );
        assert_eq!(free_buffer([true, true], 1), None);

        let base = tree("owed");
        let mut st = state(Mode::Browse, &base);
        st.busy = [true, true];
        // With no frame owed a release is only bookkeeping.
        assert!(!st.buffer_released(0, st.generation));
        assert_eq!(st.busy, [false, true]);

        // With one owed, it is the moment to draw it.
        st.busy = [true, true];
        st.dirty = true;
        assert!(
            st.buffer_released(1, st.generation),
            "the owed frame was dropped for good"
        );
        assert_eq!(st.busy, [true, false]);

        // A buffer of a pool that has been torn down carries an index into a
        // pool that no longer exists: it must not clear anything.
        st.busy = [true, true];
        st.dirty = true;
        assert!(!st.buffer_released(0, st.generation.wrapping_sub(1)));
        assert_eq!(st.busy, [true, true], "a stale buffer freed a live one");
        assert!(!st.buffer_released(BUFFERS, st.generation));
        assert_eq!(st.busy, [true, true]);

        // Claiming is the other half: it books the debt when there is no
        // buffer, and clears it when there is.
        st.busy = [true, true];
        st.dirty = false;
        assert_eq!(st.claim_buffer(), None);
        assert!(st.dirty, "the frame was dropped without being remembered");
        st.busy = [true, false];
        assert_eq!(st.claim_buffer(), Some(1));
        assert!(!st.dirty, "a drawn frame is not still owed");

        // A new pool: the owed frame belonged to the old one, and the indices
        // in `busy` point into buffers that no longer exist.
        st.busy = [true, true];
        st.dirty = true;
        st.next = 1;
        st.pool_replaced();
        assert_eq!(st.busy, [false, false]);
        assert!(
            !st.dirty,
            "a debt from the old pool survived into the new one"
        );
        assert_eq!(st.next, 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_start_directory_that_is_not_there_falls_back_instead_of_failing() {
        // A caller's suggestion is not worth failing the whole chooser over:
        // a portal passing a remembered directory that has since been deleted
        // must still get a chooser.
        let c = Cli {
            mode: Some(Mode::Open),
            dir: Some("/no/existe/en/absoluto".into()),
            ..Cli::default()
        };
        let st = new_state(&c);
        assert!(
            files::read_dir_sorted(&st.dir).is_ok(),
            "it stayed in a directory it cannot read: {}",
            st.dir
        );
        assert!(!st.note.is_empty(), "it fell back without saying why");
        // And a directory that IS there is used as given.
        let base = tree("start");
        let st = state(Mode::Open, &files::join_child(&base, "sub"));
        assert_eq!(st.dir, files::join_child(&base, "sub"));
        assert!(st.note.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn opening_a_file_nothing_handles_says_so_instead_of_launching_nothing() {
        // `spawn_detached` is a double-fork into `sh -c`, so a missing program
        // is an `exit 127` in a process nobody is watching: the file would
        // appear to open into nothing at all.
        let base = tree("open");
        let mut st = state(Mode::Browse, &base);
        st.openers = Vec::new();
        let path = files::join_child(&base, "sin-extension");
        std::fs::write(&path, b"x").unwrap();
        st.launch(&path);
        assert!(!st.done, "it closed as if it had opened something");
        assert!(st.note.contains("extensi"), "{}", st.note);

        st.note.clear();
        st.launch(&files::join_child(&base, "algo.xyz"));
        assert!(!st.done);
        assert!(st.note.contains("xyz"), "{}", st.note);

        // A handler naming a program that is not installed is caught BEFORE the
        // fork, and named, so the user knows what to install.
        st.note.clear();
        st.openers = vec![("txt".into(), "programa-que-no-existe %f".into())];
        st.launch(&files::join_child(&base, "a.txt"));
        assert!(!st.done);
        assert!(st.note.contains("programa-que-no-existe"), "{}", st.note);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tecno_marks_folders_in_gold_and_the_other_looks_do_not() {
        let base = tree("gold");
        let golds = |look: Look| {
            let mut st = state(Mode::Browse, &base);
            st.look = look;
            st.pal = palette(look);
            let (w, h) = (900usize, 560usize);
            let mut cv = Canvas::new(w, h);
            let _ = draw_browser(&mut cv, w, h, &st);
            let mut px = vec![0u8; w * h * 4];
            assert!(cv.blit_xrgb(&mut px));
            px.chunks(4).filter(|p| (p[2], p[1], p[0]) == GOLD).count()
        };
        assert!(golds(Look::Tecno) > 20, "no gold folder in tecno");
        assert_eq!(golds(Look::Eclipse), 0, "gold leaked into eclipse");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_the_rows_that_were_drawn_can_be_clicked() {
        // Drawing and hit testing used to disagree: the rows drawn were
        // `rows_fitting(ph)` and the rows hittable were `MAX_ROWS`, so on a
        // short output a click in the panel below the last VISIBLE row
        // resolved to a row that is not on the screen -- and opened a file the
        // user could not see. One number, handed back by the frame that drew
        // it, is what makes that impossible.
        let base = tree("click");
        let mut st = state(Mode::Browse, &base);
        // A short output, where the panel cannot fit the row cap.
        let (w, h) = (1280usize, 300usize);
        let mut cv = Canvas::new(w, h);
        let f = draw_browser(&mut cv, w, h, &st);
        st.take_frame(f.clone());
        let (row0, fits) = (f.row0_y, f.rows_shown);
        let (lx, _, lw, _) = f.list;
        assert!(fits < MAX_ROWS, "this output must be too short for the cap");
        assert!(st.rows.len() > fits, "and the listing must overflow it");

        let x = (lx + lw / 2) as f64;
        // Every drawn row is hittable, in its own band.
        for n in 0..fits {
            let y = (row0 + n as i32 * ROW_H + ROW_H / 2) as f64;
            assert_eq!(st.row_at(x, y), Some(n), "drawn row {n} is not clickable");
        }
        // And the first row past them is not, although the click is still
        // inside the panel.
        let past = (row0 + fits as i32 * ROW_H + 2) as f64;
        assert!(st.inside_panel(x, past), "the click must land in the panel");
        assert_eq!(st.row_at(x, past), None, "an undrawn row was clickable");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_browser_draws_without_a_compositor_at_every_size_it_accepts() {
        // The offline path and the compositor path share `draw_browser`, so a
        // size that panics here panics on screen. `panic = "abort"` in release
        // makes that a dead desktop, not a dead client.
        let base = tree("draw");
        for mode in [Mode::Browse, Mode::Open, Mode::OpenMany, Mode::Save] {
            let mut st = state(mode, &base);
            st.note = "un aviso bastante largo que no cabe de ninguna manera".into();
            st.field = "x".repeat(MAX_FIELD);
            st.refilter();
            for (w, h) in [
                (320, 240),
                (640, 480),
                (1280, 720),
                (1920, 1080),
                (3840, 160),
            ] {
                let mut cv = Canvas::new(w, h);
                let f = draw_browser(&mut cv, w, h, &st);
                let ((px, py, pw, ph), row0, fits) = (f.panel, f.row0_y, f.rows_shown);
                // Every place and button drawn is inside the panel.
                for r in f.places.iter().chain(f.buttons.iter().map(|(r, _)| r)) {
                    assert!(
                        r.0 >= px && r.1 >= py && r.0 + r.2 <= px + pw && r.1 + r.3 <= py + ph,
                        "{mode:?} {w}x{h}: {r:?} outside the panel"
                    );
                }
                assert!(
                    (1..=MAX_ROWS).contains(&fits),
                    "{mode:?} {w}x{h}: {fits} rows"
                );
                assert!(
                    row0 + fits as i32 * ROW_H <= py + ph - FOOT_H,
                    "{mode:?} {w}x{h}: {fits} rows run into the footer"
                );
                assert!(px >= 0 && py >= 0 && pw > 0 && ph > 0, "{mode:?} {w}x{h}");
                assert!(
                    row0 > py && row0 < py + ph,
                    "{mode:?} {w}x{h}: rows at {row0}"
                );
                let mut buf = vec![0u8; w * h * 4];
                assert!(cv.blit_argb(&mut buf), "{mode:?} {w}x{h}");
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── the design: rail, buttons, overwrite, contrast ───────────────────────

    /// A frame drawn at 1280x720, kept the way `render` keeps it.
    fn draw(st: &mut State) -> Frame {
        let mut cv = Canvas::new(1280, 720);
        let f = draw_browser(&mut cv, 1280, 720, st);
        st.take_frame(f.clone());
        f
    }

    fn centre(r: Rect) -> (f64, f64) {
        ((r.0 + r.2 / 2) as f64, (r.1 + r.3 / 2) as f64)
    }

    #[test]
    fn saving_over_an_existing_file_takes_a_second_enter() {
        let base = tree("overwrite");
        let mut st = state(Mode::Save, &base);
        st.field = "a.txt".into();
        apply(&mut st, Action::Commit);
        assert!(!st.done, "one Enter replaced an existing file");
        assert!(st.armed());
        assert!(
            st.note.contains(st.lang.s(Key::file_exists_enter_again)),
            "{}",
            st.note
        );
        assert_eq!(primary_label(&st), st.lang.s(Key::file_btn_replace));
        apply(&mut st, Action::Commit);
        assert!(st.done);
        assert_eq!(st.picked, vec![files::join_child(&base, "a.txt")]);

        // Typing disarms it: the warning was about a name no longer there.
        let mut st = state(Mode::Save, &base);
        st.field = "a.tx".into();
        apply(&mut st, Action::Type('t'));
        apply(&mut st, Action::Commit);
        apply(&mut st, Action::Rub);
        apply(&mut st, Action::Type('t'));
        assert!(!st.armed());
        apply(&mut st, Action::Commit);
        assert!(!st.done, "an edit kept the overwrite armed");

        // Clicking the file is pointing at it: armed at once, one Enter.
        let mut st = state(Mode::Save, &base);
        let row = row_of(&st, "a.txt");
        st.activate(row);
        assert!(st.armed());
        apply(&mut st, Action::Commit);
        assert_eq!(st.picked, vec![files::join_child(&base, "a.txt")]);

        // A directory's name is not a file to save as.
        let mut st = state(Mode::Save, &base);
        st.field = "sub".into();
        apply(&mut st, Action::Commit);
        apply(&mut st, Action::Commit);
        assert!(!st.done, "saved over a directory");
        assert!(st.note.contains(st.lang.s(Key::file_is_a_folder)), "{}", st.note);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_click_on_a_place_goes_there_and_rows_are_not_under_the_rail() {
        let base = tree("rail");
        let mut st = state(Mode::Browse, &base);
        st.places = vec![
            files::Place {
                label: "Inicio".into(),
                path: base.clone(),
            },
            files::Place {
                label: "sub".into(),
                path: files::join_child(&base, "sub"),
            },
        ];
        let f = draw(&mut st);
        assert_eq!(f.places.len(), 2, "every place fits a 720-line panel");
        let (lx, ..) = f.list;
        assert!(
            f.places.iter().all(|r| r.0 + r.2 <= lx),
            "the rail overlaps the list"
        );
        // Under the rail, at a row's height, is no row.
        let (x, _) = centre(f.places[0]);
        let y = (f.row0_y + ROW_H / 2) as f64;
        assert_eq!(st.row_at(x, y), None);

        let (x, y) = centre(f.places[1]);
        assert_eq!(st.place_at(x, y), Some(1));
        st.go_place(1);
        assert_eq!(st.dir, files::join_child(&base, "sub"));

        // A narrow panel has no rail, and the rows take the whole width.
        let mut cv = Canvas::new(500, 720);
        let f = draw_browser(&mut cv, 500, 720, &st);
        assert!(f.places.is_empty());
        assert_eq!(f.list.0, f.panel.0 + PAD);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_the_choosers_have_buttons_and_they_do_what_their_keys_do() {
        let base = tree("buttons");
        let mut st = state(Mode::Browse, &base);
        assert!(draw(&mut st).buttons.is_empty());

        // Cancel is Esc: a chooser that answers nothing.
        let mut st = state(Mode::Open, &base);
        let f = draw(&mut st);
        let kinds: Vec<Button> = f.buttons.iter().map(|(_, b)| *b).collect();
        assert_eq!(kinds, [Button::Cancel, Button::Primary]);
        let (x, y) = centre(f.buttons[0].0);
        assert_eq!(st.button_at(x, y), Some(Button::Cancel));
        st.press(Button::Cancel);
        assert!(st.done && st.picked.is_empty());

        // The primary is Enter on the selection.
        let mut st = state(Mode::Open, &base);
        st.sel = row_of(&st, "a.txt");
        st.press(Button::Primary);
        assert_eq!(st.picked, vec![files::join_child(&base, "a.txt")]);

        // And in `--multiple` it counts what is marked.
        let mut st = state(Mode::OpenMany, &base);
        st.sel = row_of(&st, "a.txt");
        apply(&mut st, Action::Mark);
        assert_eq!(primary_label(&st), format!("{} 1", st.lang.s(Key::file_btn_open)));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// WCAG relative luminance of an sRGB colour.
    fn luminance((r, g, b): Rgb) -> f64 {
        let ch = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    }

    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn a_selected_row_and_a_button_are_readable_in_every_look() {
        // KDE selected with its light accent and white text: 2.3:1.
        for (name, p) in [
            ("kde", &PAL_KDE),
            ("eclipse", &PAL_ECLIPSE),
            ("win11", &PAL_WIN11),
        ] {
            let sel = contrast(p.sel_text, p.sel);
            assert!(sel >= 4.5, "{name}: selected row {sel:.2}:1");
            let btn = contrast(p.on_accent, p.accent);
            assert!(btn >= 4.5, "{name}: primary button {btn:.2}:1");
        }
        assert!(contrast(ON_WARN, WARN) >= 4.5);
    }
}
