//! lunarpanel — the control panel of Eclipse OS (Super+I, or "Panel de
//! control" in the launcher).
//!
//! Nothing on this screen is laid out by hand. The settings are DATA, blocks
//! in `claves.d` (see `lunarbar::claves`), and everything below is computed
//! from them: the pages are the groups in the order they first appear, each
//! row's control comes from its type and its values (chips when they fit,
//! a stepper when they do not, a switch, a segmented meter, a button), every
//! width is the text it holds rounded up to the 4 px grid, and the label
//! column is as wide as the longest label on the page. Adding a setting is
//! writing a block; this file does not change.
//!
//! Each row says which file and key it writes. A change is held (gold) until
//! Enter or APLICAR, and Esc or DESHACER drops it. Typing searches every
//! setting, in both languages.
//!
//! `--dump PATH:WxH [GRUPO]` renders the shipped blocks to a raw ARGB8888
//! file; `--grupo ID` opens on that page.

use std::os::fd::{AsFd, AsRawFd};

use lunarbar::claves::{self, Catalogo, Clave, Tipo};
use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::glass::{self, ink};
use lunarbar::i18n::{Key, Lang};
use lunarbar::keys::{
    key_char, BTN_LEFT, KEY_BACKSPACE_WL, KEY_DOWN_WL, KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL,
    KEY_LEFT_WL, KEY_PGDN_WL, KEY_PGUP_WL, KEY_RIGHT_WL, KEY_TAB_WL, KEY_UP_WL,
};
use lunarbar::proc::{self, map_shm_pool};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

#[path = "lunarpanel/disp.rs"]
mod disp;

// ── the measures the computation starts from ─────────────────────────────────

const MARGIN: i32 = 48;
const PANEL_MAX_W: i32 = 1040;
const PANEL_MAX_H: i32 = 680;
const HEAD_H: i32 = 48;
const SEARCH_H: i32 = 28;
const FOOT_H: i32 = 56;
const PAD: i32 = 16;
const NAV_ROW_H: i32 = 32;
const NAV_MIN_W: i32 = 192;
/// Where a page's name starts, after its mark.
const NAV_TEXT_X: i32 = 28;
const ROW_H: i32 = 60;
/// A row's second line (origin, help), below the control's focus brackets.
const LINE2_Y: i32 = 40;
const CTRL_H: i32 = 24;
const CHIP_PAD: i32 = 20;
const CHIP_GAP: i32 = 4;
const LABEL_MIN_W: i32 = 160;
const SWITCH_W: i32 = 44;
const SWITCH_H: i32 = 20;
const SEG_W: i32 = 8;
const SEG_GAP: i32 = 4;
const VAL_W: i32 = 56;
const STEP_W: i32 = 24;
const BTN_W: i32 = 120;
const BTN_H: i32 = 32;
/// More segments than this and a meter stops being readable at a glance.
const MAX_SEGS: i64 = 24;
/// More values than this and a row of chips stops being a choice at a glance.
const MAX_CHIPS: usize = 6;

/// The evdev code of the space bar.
const KEY_SPACE: u32 = 57;

type Rect = (i32, i32, i32, i32);

fn hit(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

/// Up to the next grid line.
fn up4(v: i32) -> i32 {
    (v + 3) / 4 * 4
}

fn fit(s: &str, px: i32) -> String {
    let cols = (px / GLYPH_W).max(1) as usize;
    if s.chars().count() <= cols {
        return s.to_string();
    }
    let mut t: String = s.chars().take(cols.saturating_sub(1)).collect();
    t.push('…');
    t
}

// ── the computation ──────────────────────────────────────────────────────────

/// What a row's control turned out to be, with where its parts are.
#[derive(Debug, Clone, PartialEq)]
enum Control {
    /// One chip per value, in the data's order.
    Fichas(Vec<Rect>),
    /// Too many values (or too wide) for chips: `‹ value ›`.
    Lista {
        prev: Rect,
        caja: Rect,
        next: Rect,
    },
    Interruptor(Rect),
    /// One segment per step, then the number.
    Medidor {
        segs: Vec<Rect>,
        caja: Rect,
    },
    Boton(Rect),
}

#[derive(Debug, Clone)]
struct Fila {
    /// Index into `Catalogo::claves`.
    clave: usize,
    rect: Rect,
    control: Control,
}

#[derive(Debug)]
struct Layout {
    panel: Rect,
    search: Rect,
    nav: Vec<Rect>,
    nav_w: i32,
    /// The label column, computed from the page's longest label.
    label_w: i32,
    /// The body's top and height (rows scroll inside it).
    body: Rect,
    filas: Vec<Fila>,
    /// How many rows fit at once.
    caben: usize,
    apply: Rect,
    revert: Rect,
}

fn nombre_grupo(cat: &Catalogo, g: usize, en: bool) -> String {
    cat.grupos
        .get(g)
        .map(|x| x.nombre(en).to_uppercase())
        .unwrap_or_default()
}

fn etiqueta_valor(c: &Clave, v: &str, lang: Lang) -> String {
    match c.tipo {
        Tipo::SiNo if c.valores.first().is_some_and(|x| x.v == v) => lang.s(Key::panel_on).into(),
        Tipo::SiNo => lang.s(Key::hunt_sched_off).into(),
        _ => c.etiqueta(v, lang == Lang::En).to_string(),
    }
}

/// The control a setting gets in `room` pixels, placed at `(x, y)`.
fn control(c: &Clave, x: i32, y: i32, room: i32, lang: Lang) -> Control {
    let en = lang == Lang::En;
    match &c.tipo {
        Tipo::Elegir => {
            let ws: Vec<i32> = c
                .valores
                .iter()
                .map(|v| up4(Canvas::text_width(if en { &v.en } else { &v.es }) + CHIP_PAD))
                .collect();
            let total: i32 = ws.iter().sum::<i32>() + CHIP_GAP * (ws.len() as i32 - 1).max(0);
            if ws.len() <= MAX_CHIPS && total <= room {
                let mut at = x;
                let rects = ws
                    .iter()
                    .map(|w| {
                        let r = (at, y, *w, CTRL_H);
                        at += w + CHIP_GAP;
                        r
                    })
                    .collect();
                Control::Fichas(rects)
            } else {
                // The box holds the widest label AND its `n/m` count.
                let n = ws.len();
                let count = Canvas::text_width(&format!("{n}/{n}"));
                let widest = up4(ws.iter().copied().max().unwrap_or(0) + count + 8);
                // Down to the grid, so the box never pokes past the room.
                let hi = ((room - 2 * (STEP_W + CHIP_GAP)) / 4 * 4).max(VAL_W);
                let caja_w = widest.max(VAL_W * 2).min(hi);
                let caja = (x + STEP_W + CHIP_GAP, y, caja_w, CTRL_H);
                Control::Lista {
                    prev: (x, y, STEP_W, CTRL_H),
                    caja,
                    next: (caja.0 + caja_w + CHIP_GAP, y, STEP_W, CTRL_H),
                }
            }
        }
        Tipo::SiNo => Control::Interruptor((x, y + (CTRL_H - SWITCH_H) / 2, SWITCH_W, SWITCH_H)),
        Tipo::Numero { min, max, paso } => {
            let steps = ((max - min) / paso + 1).min(MAX_SEGS) as i32;
            let fits = ((room - VAL_W - 2 * PAD) / (SEG_W + SEG_GAP)).max(1);
            let n = steps.min(fits);
            let segs: Vec<Rect> = (0..n)
                .map(|k| (x + k * (SEG_W + SEG_GAP), y + 4, SEG_W, CTRL_H - 8))
                .collect();
            let after = x + n * (SEG_W + SEG_GAP) + PAD - SEG_GAP;
            Control::Medidor {
                segs,
                caja: (after, y, VAL_W, CTRL_H),
            }
        }
        Tipo::Abrir | Tipo::Dispositivos => {
            let w = up4(Canvas::text_width(lang.s(Key::panel_open_caps)) + 2 * PAD);
            Control::Boton((x, y, w, CTRL_H))
        }
    }
}

/// The whole screen, from the data, the output size, which rows are shown
/// (a page or a search) and the first one in view.
fn layout(w: i32, h: i32, cat: &Catalogo, shown: &[usize], top: usize, lang: Lang) -> Layout {
    let en = lang == Lang::En;
    let pw = PANEL_MAX_W.min(w - 2 * MARGIN).max(320.min(w));
    let ph = PANEL_MAX_H
        .min(h - 2 * MARGIN)
        .max((HEAD_H + FOOT_H + ROW_H).min(h));
    let (px, py) = ((w - pw) / 2, ((h - ph) / 2).max(0));

    // The page list is as wide as its longest name and its count.
    let longest = (0..cat.grupos.len())
        .map(|g| Canvas::text_width(&nombre_grupo(cat, g, en)))
        .max()
        .unwrap_or(0);
    let nav_w = up4(NAV_TEXT_X + longest + 12 + Canvas::text_width("00") + PAD)
        .max(NAV_MIN_W)
        .min(pw / 3);
    let search = (px + PAD, py + HEAD_H + PAD / 2, pw - 2 * PAD, SEARCH_H);
    let body_y = search.1 + SEARCH_H + PAD / 2;
    let body = (px + nav_w, body_y, pw - nav_w, py + ph - FOOT_H - body_y);
    let nav = (0..cat.grupos.len())
        .map(|g| {
            (
                px + 1,
                body_y + PAD / 2 + g as i32 * NAV_ROW_H,
                nav_w - 2,
                NAV_ROW_H,
            )
        })
        .collect();

    // The label column is the page's longest name or origin, on the grid,
    // never less than a minimum and never more than half the room.
    let rx = body.0 + PAD + 4;
    let rw = body.2 - 2 * PAD - 8;
    let widest = shown
        .iter()
        .map(|&i| {
            let c = &cat.claves[i];
            Canvas::text_width(c.nombre(en)).max(Canvas::text_width(&c.origen()))
        })
        .max()
        .unwrap_or(0);
    // Capped at half the room, down to the grid: a long origin (rboot.conf
    // on the ESP) must not push the controls off the panel.
    let half = (rw / 2) / 4 * 4;
    let label_w = up4(widest + 2 * PAD).clamp(LABEL_MIN_W, half.max(LABEL_MIN_W));

    let caben = (((body.3 - PAD) / ROW_H).max(1)) as usize;
    let first_y = body.1 + PAD / 2 + NAV_ROW_H;
    let filas = shown
        .iter()
        .skip(top)
        .take(caben.saturating_sub(1).max(1))
        .enumerate()
        .map(|(n, &i)| {
            let y = first_y + n as i32 * ROW_H;
            let cx = rx + label_w;
            Fila {
                clave: i,
                rect: (rx, y, rw, ROW_H),
                control: control(&cat.claves[i], cx, y + 8, rw - label_w, lang),
            }
        })
        .collect();

    let fy = py + ph - FOOT_H + (FOOT_H - BTN_H) / 2;
    let apply = (px + pw - PAD - BTN_W, fy, BTN_W, BTN_H);
    let revert = (apply.0 - 12 - BTN_W, fy, BTN_W, BTN_H);
    Layout {
        panel: (px, py, pw, ph),
        search,
        nav,
        nav_w,
        label_w,
        body,
        filas,
        caben: caben.saturating_sub(1).max(1),
        apply,
        revert,
    }
}

// ── the state the screen shows ───────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tono {
    Plano,
    Bien,
    Mal,
}

struct Panel {
    cat: Catalogo,
    /// The value in force, one per setting.
    actual: Vec<String>,
    /// A value chosen and not applied yet.
    pendiente: Vec<Option<String>>,
    grupo: usize,
    /// The focused row, as a position in [`Panel::shown`].
    foco: usize,
    top: usize,
    busca: String,
    status: String,
    tono: Tono,
    lang: Lang,
    /// The devices page's rows and focus.
    disp: disp::Disp,
}

impl Panel {
    fn new(cat: Catalogo, actual: Vec<String>, lang: Lang) -> Self {
        let n = cat.claves.len();
        Panel {
            cat,
            actual,
            pendiente: vec![None; n],
            grupo: 0,
            foco: 0,
            top: 0,
            busca: String::new(),
            status: String::new(),
            tono: Tono::Plano,
            lang,
            disp: disp::Disp::default(),
        }
    }

    fn shown(&self) -> Vec<usize> {
        if self.busca.trim().is_empty() {
            self.cat.del_grupo(self.grupo)
        } else {
            self.cat.busca(&self.busca)
        }
    }

    fn valor(&self, i: usize) -> &str {
        self.pendiente[i].as_deref().unwrap_or(&self.actual[i])
    }

    fn pendientes(&self) -> usize {
        self.pendiente.iter().filter(|p| p.is_some()).count()
    }

    /// Holds `v` for setting `i`, or drops the hold when `v` is what is in
    /// force already.
    fn elige(&mut self, i: usize, v: String) {
        if !self.cat.claves[i].admite(&v) {
            return;
        }
        self.pendiente[i] = (v != self.actual[i]).then_some(v);
        self.status.clear();
    }

    fn mueve(&mut self, d: i32, caben: usize) {
        let n = self.shown().len();
        if n == 0 {
            return;
        }
        self.foco = (self.foco as i32 + d).clamp(0, n as i32 - 1) as usize;
        if self.foco < self.top {
            self.top = self.foco;
        } else if self.foco >= self.top + caben {
            self.top = self.foco + 1 - caben;
        }
    }

    fn pagina(&mut self, g: usize) {
        if g < self.cat.grupos.len() {
            self.grupo = g;
            self.foco = 0;
            self.top = 0;
            self.busca.clear();
            self.carga_disp();
        }
    }

    fn deshaz(&mut self) {
        self.pendiente.iter_mut().for_each(|p| *p = None);
        self.status = self.lang.s(Key::panel_undone).into();
        self.tono = Tono::Plano;
    }

    /// Applies every held change in the data's order, then reads every value
    /// back (one command can move another setting: the keyboard switch moves
    /// the language). Returns whether one of them asks for the panel to be
    /// opened again.
    fn aplica(&mut self) -> bool {
        let mut hechos = 0;
        let mut fallo = None;
        let mut reabrir = false;
        let mut al_reiniciar = false;
        for i in 0..self.cat.claves.len() {
            let Some(v) = self.pendiente[i].take() else {
                continue;
            };
            match claves::aplica(&self.cat.claves[i], &v) {
                Ok(()) => {
                    hechos += 1;
                    reabrir |= self.cat.claves[i].reabrir;
                    al_reiniciar |= self.cat.claves[i].reinicio;
                }
                Err(e) => {
                    eprintln!("lunarpanel: {e}");
                    fallo.get_or_insert(e);
                }
            }
        }
        self.actual = self.cat.claves.iter().map(claves::valor_actual).collect();
        match fallo {
            Some(e) => {
                self.status = e;
                self.tono = Tono::Mal;
                false
            }
            None => {
                self.status = format!("{} · {hechos}", self.lang.s(Key::hunt_applied));
                if al_reiniciar {
                    self.status += &format!(" · {}", self.lang.s(Key::panel_on_reboot));
                }
                self.tono = Tono::Bien;
                reabrir
            }
        }
    }
}

// ── drawing ──────────────────────────────────────────────────────────────────

fn frame(cv: &mut Canvas, r: Rect, c: Rgb) {
    glass::outline(cv, r.0, r.1, r.2, r.3, c);
}

fn chip(cv: &mut Canvas, r: Rect, label: &str, on: bool, held: bool) {
    let edge = if held {
        ink().gold
    } else if on {
        ink().cyan
    } else {
        ink().hairline
    };
    glass::cell(
        cv,
        r.0,
        r.1,
        r.2,
        r.3,
        if on { edge } else { ink().well },
        if on { 0.16 } else { 1.0 },
    );
    frame(cv, r, edge);
    let t = fit(label, r.2 - 8);
    let tx = r.0 + (r.2 - Canvas::text_width(&t)) / 2;
    let ty = r.1 + (r.3 - GLYPH_H) / 2;
    if on {
        cv.text_bold(&t, tx, ty, edge);
    } else {
        cv.text(&t, tx, ty, ink().muted);
    }
}

fn button(cv: &mut Canvas, r: Rect, label: &str, lit: bool) {
    let c = if lit { ink().gold } else { ink().well };
    glass::cell(cv, r.0, r.1, r.2, r.3, c, if lit { 0.14 } else { 1.0 });
    frame(cv, r, if lit { ink().gold } else { ink().hairline });
    let tx = r.0 + (r.2 - Canvas::text_width(label)) / 2;
    let ty = r.1 + (r.3 - GLYPH_H) / 2;
    if lit {
        cv.text_bold(label, tx, ty, ink().gold);
    } else {
        cv.text(label, tx, ty, ink().muted);
    }
}

fn draw_control(cv: &mut Canvas, f: &Fila, c: &Clave, v: &str, held: bool, lang: Lang) {
    let en = lang == Lang::En;
    let accent = if held { ink().gold } else { ink().cyan };
    match &f.control {
        Control::Fichas(rects) => {
            for (r, val) in rects.iter().zip(&c.valores) {
                let on = val.v == v;
                chip(cv, *r, if en { &val.en } else { &val.es }, on, held && on);
            }
        }
        Control::Lista { prev, caja, next } => {
            chip(cv, *prev, "‹", false, false);
            chip(cv, *next, "›", false, false);
            glass::cell(cv, caja.0, caja.1, caja.2, caja.3, ink().well, 1.0);
            frame(cv, *caja, accent);
            let at = c.valores.iter().position(|x| x.v == v).unwrap_or(0);
            let count = format!("{}/{}", at + 1, c.valores.len());
            let cw = Canvas::text_width(&count);
            let ty = caja.1 + (CTRL_H - GLYPH_H) / 2;
            cv.text(
                &fit(c.etiqueta(v, en), caja.2 - cw - 24),
                caja.0 + 8,
                ty,
                ink().text,
            );
            cv.text(&count, caja.0 + caja.2 - cw - 8, ty, ink().dim);
        }
        Control::Interruptor(r) => {
            let on = c.valores.first().is_some_and(|x| x.v == v);
            let col = if held {
                ink().gold
            } else if on {
                ink().ok
            } else {
                ink().muted
            };
            glass::cell(cv, r.0, r.1, r.2, r.3, ink().well, 1.0);
            frame(cv, *r, if on || held { col } else { ink().hairline });
            let kw = (r.2 - 4) / 2;
            let kx = if on { r.0 + r.2 - 2 - kw } else { r.0 + 2 };
            cv.fill_rect_a(kx, r.1 + 3, kw, r.3 - 6, col, 1.0);
            let word = etiqueta_valor(c, v, lang);
            cv.text(&word, r.0 + r.2 + 12, r.1 + (r.3 - GLYPH_H) / 2, col);
        }
        Control::Medidor { segs, caja } => {
            let Tipo::Numero { min, max, .. } = c.tipo else {
                return;
            };
            let n = v.parse::<i64>().unwrap_or(min);
            let lit = if max > min {
                (((n - min) as f64 / (max - min) as f64) * segs.len() as f64).round() as usize
            } else {
                0
            };
            for (k, r) in segs.iter().enumerate() {
                let col = if k < lit { accent } else { ink().well };
                cv.fill_rect_a(r.0, r.1, r.2, r.3, col, 1.0);
            }
            glass::cell(cv, caja.0, caja.1, caja.2, caja.3, ink().well, 1.0);
            frame(cv, *caja, ink().hairline);
            let tw = Canvas::text_width(v);
            cv.text(
                v,
                caja.0 + (caja.2 - tw) / 2,
                caja.1 + (CTRL_H - GLYPH_H) / 2,
                accent,
            );
            let range = format!("{min}..{max}");
            cv.text(
                &range,
                caja.0 + caja.2 + 12,
                caja.1 + (CTRL_H - GLYPH_H) / 2,
                ink().dim,
            );
        }
        Control::Boton(r) => button(cv, *r, lang.s(Key::panel_open_caps), false),
    }
}

/// Where a row's control ends, for the brackets round it.
fn control_box(ctl: &Control) -> Rect {
    let span = |a: Rect, b: Rect| (a.0, a.1, b.0 + b.2 - a.0, (b.1 + b.3).max(a.1 + a.3) - a.1);
    match ctl {
        Control::Fichas(r) if !r.is_empty() => span(r[0], r[r.len() - 1]),
        Control::Fichas(_) => (0, 0, 0, 0),
        Control::Lista { prev, next, .. } => span(*prev, *next),
        Control::Interruptor(r) | Control::Boton(r) => *r,
        Control::Medidor { segs, caja } => span(segs.first().copied().unwrap_or(*caja), *caja),
    }
}

fn draw(cv: &mut Canvas, w: i32, h: i32, p: &Panel) {
    let en = p.lang == Lang::En;
    let shown = p.shown();
    let l = layout(w, h, &p.cat, &shown, p.top, p.lang);
    let (px, py, pw, ph) = l.panel;
    cv.fill_rect_a(0, 0, w, h, ink().ground, ink().veil);
    glass::panel(cv, px, py, pw, ph, ink().surface);
    frame(cv, l.panel, ink().hairline);
    glass::brackets(cv, px, py, pw, ph, 16, 2, ink().gold);

    // Head: the crescent, the title, the keys.
    let hy = py + (HEAD_H - GLYPH_H) / 2;
    let mx = px + PAD + 6;
    let my = py + HEAD_H / 2;
    cv.crescent(mx - 6, my - 6, 12, ink().gold);
    let tw = cv.text_bold(p.lang.s(Key::panel_title), px + PAD + 20, hy, ink().cyan);
    cv.text("eclipse os", px + PAD + 32 + tw, hy, ink().dim);
    let keys = p.lang.s(Key::panel_keys);
    let kw = Canvas::text_width(keys);
    if kw < pw - tw - 200 {
        cv.text(keys, px + pw - PAD - kw, hy, ink().muted);
    }
    cv.hline(px + PAD, py + HEAD_H - 1, pw - 2 * PAD, ink().hairline, 1.0);

    // Search.
    let s = l.search;
    glass::cell(cv, s.0, s.1, s.2, s.3, ink().well, 1.0);
    frame(
        cv,
        s,
        if p.busca.is_empty() {
            ink().hairline
        } else {
            ink().cyan
        },
    );
    let sy = s.1 + (SEARCH_H - GLYPH_H) / 2;
    cv.text_bold("›", s.0 + 10, sy, ink().cyan);
    if p.busca.is_empty() {
        let hint = p
            .lang
            .s(Key::panel_search_in)
            .replace("{}", &p.cat.claves.len().to_string());
        cv.text(&hint, s.0 + 28, sy, ink().dim);
    } else {
        let adv = cv.text(&p.busca, s.0 + 28, sy, ink().text);
        cv.fill_rect_a(s.0 + 29 + adv, s.1 + 6, 2, SEARCH_H - 12, ink().cyan, 1.0);
    }

    // Pages, computed from the groups; the count is how many settings each holds.
    let (bx, by, _, bh) = l.body;
    cv.fill_rect_a(bx, by, 1, bh, ink().hairline, 1.0);
    for (g, r) in l.nav.iter().enumerate() {
        let on = g == p.grupo && p.busca.is_empty();
        if on {
            glass::cell(cv, r.0, r.1, r.2, r.3, ink().cyan, 0.08);
            cv.fill_rect_a(r.0, r.1, 2, r.3, ink().cyan, 1.0);
        }
        let ty = r.1 + (NAV_ROW_H - GLYPH_H) / 2;
        cv.fill_rect_a(
            r.0 + PAD - 4,
            ty + 3,
            8,
            8,
            if on { ink().cyan } else { ink().hairline },
            1.0,
        );
        let count_room = 12 + Canvas::text_width("00") + PAD;
        let name = fit(
            &nombre_grupo(&p.cat, g, en),
            l.nav_w - NAV_TEXT_X - count_room,
        );
        cv.text(
            &name,
            r.0 + NAV_TEXT_X,
            ty,
            if on { ink().text } else { ink().muted },
        );
        let held = p.cat.del_grupo(g).iter().any(|&i| p.pendiente[i].is_some());
        let count = format!("{:02}", p.cuenta_grupo(g));
        let cw = Canvas::text_width(&count);
        let col = if held {
            ink().gold
        } else if on {
            ink().cyan
        } else {
            ink().dim
        };
        cv.text(&count, r.0 + r.2 - PAD - cw, ty, col);
    }

    // The devices page draws its own body and foot.
    if let Some(ci) = p.pagina_disp() {
        disp::draw(cv, &l, p, ci);
        return;
    }

    // The page.
    let rx = bx + PAD + 4;
    let rw = l.body.2 - 2 * PAD - 8;
    let sec_y = by + PAD / 2 + (NAV_ROW_H - GLYPH_H) / 2;
    let title = if p.busca.is_empty() {
        nombre_grupo(&p.cat, p.grupo, en)
    } else {
        p.lang.s(Key::panel_results).to_uppercase()
    };
    let stw = cv.text_bold(&title, rx, sec_y, ink().text);
    let n = format!("{:02}", shown.len());
    let nw = Canvas::text_width(&n);
    cv.hline(
        rx + stw + 12,
        sec_y + GLYPH_H / 2,
        rw - stw - nw - 24,
        ink().hairline,
        1.0,
    );
    cv.text(&n, rx + rw - nw, sec_y, ink().muted);
    if shown.is_empty() {
        cv.text(
            p.lang.s(Key::panel_no_results),
            rx,
            sec_y + NAV_ROW_H + 8,
            ink().muted,
        );
    }
    for (k, f) in l.filas.iter().enumerate() {
        let c = &p.cat.claves[f.clave];
        let held = p.pendiente[f.clave].is_some();
        let focused = p.top + k == p.foco;
        let (x, y, fw, fh) = f.rect;
        if held {
            cv.fill_rect_a(x - 8, y + 8, 2, fh - 16, ink().gold, 1.0);
        }
        cv.text(
            &fit(c.nombre(en), l.label_w - PAD),
            x,
            y + 12,
            if focused { ink().text } else { ink().muted },
        );
        let src = fit(&c.origen(), l.label_w - PAD);
        cv.text(&src, x, y + LINE2_Y, ink().dim);
        draw_control(cv, f, c, p.valor(f.clave), held, p.lang);
        let cb = control_box(&f.control);
        let help_x = x + l.label_w;
        let help = if held {
            format!(
                "{} · {} {}",
                p.lang.s(Key::net_unapplied),
                p.lang.s(Key::panel_was),
                etiqueta_valor(c, &p.actual[f.clave], p.lang)
            )
        } else {
            c.ayuda(en).to_string()
        };
        cv.text(
            &fit(&help, fw - l.label_w),
            help_x,
            y + LINE2_Y,
            if held { ink().gold } else { ink().dim },
        );
        if focused {
            glass::brackets(cv, cb.0 - 4, cb.1 - 4, cb.2 + 8, cb.3 + 8, 8, 2, ink().gold);
        }
        cv.hline(x, y + fh - 1, fw, ink().hairline, 0.5);
    }
    let more = shown.len().saturating_sub(p.top + l.filas.len());
    if more > 0 || p.top > 0 {
        let t = format!("{}–{} / {}", p.top + 1, p.top + l.filas.len(), shown.len());
        let tw = Canvas::text_width(&t);
        cv.text(&t, rx + rw - tw, by + bh - GLYPH_H - 4, ink().dim);
    }

    // Foot: what the data amounted to, or what just happened.
    let top = py + ph - FOOT_H;
    cv.hline(px + PAD, top, pw - 2 * PAD, ink().hairline, 1.0);
    let pend = p.pendientes();
    button(cv, l.revert, p.lang.s(Key::net_undo_caps), false);
    button(cv, l.apply, p.lang.s(Key::net_apply_caps), pend > 0);
    let fy = top + (FOOT_H - GLYPH_H) / 2;
    let room = l.revert.0 - px - 2 * PAD;
    if !p.status.is_empty() {
        let col = match p.tono {
            Tono::Plano => ink().muted,
            Tono::Bien => ink().cyan,
            Tono::Mal => ink().warn,
        };
        cv.text(&fit(&p.status, room), px + PAD, fy, col);
    } else {
        let mut x = px + PAD;
        let mut put = |cv: &mut Canvas, num: String, word: &str, col: Rgb| {
            x += cv.text(&num, x, fy, col);
            x += cv.text(&format!(" {word}   "), x, fy, ink().muted);
        };
        put(
            cv,
            p.cat.claves.len().to_string(),
            p.lang.s(Key::panel_settings),
            ink().cyan,
        );
        put(
            cv,
            p.cat.grupos.len().to_string(),
            p.lang.s(Key::panel_groups),
            ink().cyan,
        );
        if !p.cat.avisos.is_empty() {
            put(
                cv,
                p.cat.avisos.len().to_string(),
                p.lang.s(Key::panel_warnings),
                ink().warn,
            );
        }
        if pend > 0 {
            put(
                cv,
                pend.to_string(),
                p.lang.s(Key::net_unapplied),
                ink().gold,
            );
        }
    }
}

// ── wayland ──────────────────────────────────────────────────────────────────

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    w: u32,
    h: u32,
    map: *mut u8,
    len: usize,
    buffer: Option<wl_buffer::WlBuffer>,
    ptr: (f64, f64),
    shift: bool,
    panel: Panel,
    done: bool,
    /// Set when an applied setting asks for the panel to open again.
    reabrir: bool,
}

impl State {
    fn new(panel: Panel) -> Self {
        State {
            compositor: None,
            shm: None,
            layer_shell: None,
            seat: None,
            keyboard: None,
            pointer: None,
            surface: None,
            layer: None,
            w: 0,
            h: 0,
            map: std::ptr::null_mut(),
            len: 0,
            buffer: None,
            ptr: (0.0, 0.0),
            shift: false,
            panel,
            done: false,
            reabrir: false,
        }
    }

    fn layout(&self) -> Layout {
        let shown = self.panel.shown();
        layout(
            self.w as i32,
            self.h as i32,
            &self.panel.cat,
            &shown,
            self.panel.top,
            self.panel.lang,
        )
    }

    fn render(&mut self) {
        let (Some(surface), Some(buf)) = (self.surface.clone(), self.buffer.clone()) else {
            return;
        };
        if self.map.is_null() {
            return;
        }
        let Some(mut cv) = Canvas::try_new(self.w as usize, self.h as usize) else {
            return;
        };
        cv.set_square(!glass::on());
        draw(&mut cv, self.w as i32, self.h as i32, &self.panel);
        // SAFETY: `map` is the `len` bytes mapped for this surface's buffer.
        let data = unsafe { std::slice::from_raw_parts_mut(self.map, self.len) };
        glass::finish(&mut cv);
        if cv.blit_argb(data) {
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, self.w as i32, self.h as i32);
            surface.commit();
        }
    }

    fn apply(&mut self) {
        if self.panel.pendientes() == 0 {
            return;
        }
        if self.panel.aplica() {
            self.reabrir = true;
            self.done = true;
        }
    }

    /// The focused row's setting, if any.
    fn focused(&self) -> Option<usize> {
        self.panel.shown().get(self.panel.foco).copied()
    }

    fn step(&mut self, dir: i32) {
        let Some(i) = self.focused() else { return };
        let c = &self.panel.cat.claves[i];
        if matches!(c.tipo, Tipo::Abrir | Tipo::Dispositivos) {
            return;
        }
        let v = c.paso(self.panel.valor(i), dir);
        self.panel.elige(i, v);
    }

    fn activate(&mut self) {
        let Some(i) = self.focused() else { return };
        match self.panel.cat.claves[i].tipo {
            Tipo::Abrir | Tipo::Dispositivos => self.open(i),
            Tipo::SiNo => self.step(1),
            _ => self.apply(),
        }
    }

    fn open(&mut self, i: usize) {
        // The devices block, found by a search: its page.
        if self.panel.cat.claves[i].tipo == Tipo::Dispositivos {
            let g = self.panel.cat.claves[i].grupo.clone();
            let g = group_index(&self.panel.cat, Some(&g));
            self.panel.pagina(g);
            return;
        }
        match claves::abre(&self.panel.cat.claves[i]) {
            Ok(()) => self.done = self.panel.pendientes() == 0,
            Err(e) => {
                self.panel.status = e;
                self.panel.tono = Tono::Mal;
            }
        }
    }

    fn key(&mut self, key: u32) {
        if self.panel.pagina_disp().is_some() && self.disp_key(key) {
            self.render();
            return;
        }
        let caben = self.layout().caben;
        let groups = self.panel.cat.grupos.len().max(1);
        match key {
            KEY_ESC_WL => {
                if self.panel.pendientes() > 0 {
                    self.panel.deshaz();
                } else if !self.panel.busca.is_empty() {
                    self.panel.busca.clear();
                    self.panel.foco = 0;
                    self.panel.top = 0;
                } else {
                    self.done = true;
                }
            }
            KEY_UP_WL => self.panel.mueve(-1, caben),
            KEY_DOWN_WL => self.panel.mueve(1, caben),
            KEY_PGUP_WL => self.panel.mueve(-(caben as i32), caben),
            KEY_PGDN_WL => self.panel.mueve(caben as i32, caben),
            KEY_LEFT_WL => self.step(-1),
            KEY_RIGHT_WL => self.step(1),
            KEY_TAB_WL => {
                let g = self.panel.grupo + if self.shift { groups - 1 } else { 1 };
                self.panel.pagina(g % groups);
            }
            KEY_ENTER_WL | KEY_KPENTER_WL => self.activate(),
            KEY_SPACE if self.panel.busca.is_empty() => self.activate(),
            KEY_BACKSPACE_WL => {
                self.panel.busca.pop();
                self.panel.foco = 0;
                self.panel.top = 0;
            }
            _ => {
                if let Some(c) = key_char(key) {
                    if self.panel.busca.chars().count() < 32 {
                        self.panel.busca.push(c);
                        self.panel.foco = 0;
                        self.panel.top = 0;
                    }
                }
            }
        }
        if !self.done {
            self.render();
        }
    }

    fn click(&mut self) {
        let (x, y) = (self.ptr.0 as i32, self.ptr.1 as i32);
        let l = self.layout();
        if !hit(l.panel, x, y) {
            if self.panel.pendientes() == 0 {
                self.done = true;
            }
            return;
        }
        if self.panel.pagina_disp().is_some() && self.disp_click(x, y) {
            self.render();
            return;
        }
        if hit(l.apply, x, y) {
            self.apply();
        } else if hit(l.revert, x, y) {
            self.panel.deshaz();
        } else if let Some(g) = l.nav.iter().position(|r| hit(*r, x, y)) {
            self.panel.pagina(g);
        } else if let Some((k, f)) = l.filas.iter().enumerate().find(|(_, f)| hit(f.rect, x, y)) {
            self.panel.foco = self.panel.top + k;
            let i = f.clave;
            let c = &self.panel.cat.claves[i];
            match &f.control {
                Control::Fichas(rects) => {
                    if let Some(n) = rects.iter().position(|r| hit(*r, x, y)) {
                        let v = c.valores[n].v.clone();
                        self.panel.elige(i, v);
                    }
                }
                Control::Lista { prev, next, .. } => {
                    if hit(*prev, x, y) {
                        self.step(-1);
                    } else if hit(*next, x, y) {
                        self.step(1);
                    }
                }
                Control::Interruptor(r) => {
                    if hit(*r, x, y) {
                        self.step(1);
                    }
                }
                Control::Medidor { segs, .. } => {
                    if let (Some(n), Tipo::Numero { min, max, .. }) =
                        (segs.iter().position(|r| hit(*r, x, y)), &c.tipo)
                    {
                        let frac = (n + 1) as f64 / segs.len() as f64;
                        let v = (*min as f64 + frac * (*max - *min) as f64).round() as i64;
                        self.panel.elige(i, c.normaliza(Some(&v.to_string())));
                    }
                }
                Control::Boton(r) => {
                    if hit(*r, x, y) {
                        self.open(i);
                    }
                }
            }
        }
        if !self.done {
            self.render();
        }
    }

    fn configure(&mut self, qh: &QueueHandle<State>, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let (w, h) = (if w == 0 { 1280 } else { w }, if h == 0 { 720 } else { h });
        if fill_guard::check_buffer(w as usize, h as usize).is_err() {
            return;
        }
        if self.w != w || self.h != h {
            let Some(geom) = proc::pool_geometry(w, h) else {
                return;
            };
            let Some((map, fd)) = map_shm_pool(geom.frame_size, "lunarpanel") else {
                return;
            };
            if let Some(b) = self.buffer.take() {
                b.destroy();
            }
            if !self.map.is_null() {
                unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
            }
            let pool = shm.create_pool(fd.as_fd(), geom.frame_size as i32, qh, ());
            self.buffer = Some(pool.create_buffer(
                0,
                w as i32,
                h as i32,
                geom.stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            ));
            pool.destroy();
            self.map = map;
            self.len = geom.frame_size;
            self.w = w;
            self.h = h;
        }
        self.render();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    st.layer_shell = Some(reg.bind(name, version.min(4), qh, ()))
                }
                "wl_seat" if st.seat.is_none() => {
                    st.seat = Some(reg.bind(name, version.min(5), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        st: &mut Self,
        l: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                l.ack_configure(serial);
                st.configure(qh, width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = ev
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
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match ev {
            // Shift is bit 0 of the depressed mods in every xkb keymap.
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                st.shift = mods_depressed & 1 != 0
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => st.key(key),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        st: &mut Self,
        _: &wl_pointer::WlPointer,
        ev: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match ev {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => st.ptr = (surface_x, surface_y),
            wl_pointer::Event::Button {
                button: BTN_LEFT,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => st.click(),
            wl_pointer::Event::Axis { value, .. } => {
                let caben = st.layout().caben;
                st.panel.mueve(if value > 0.0 { 1 } else { -1 }, caben);
                st.render();
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

// ── main ─────────────────────────────────────────────────────────────────────

fn group_index(cat: &Catalogo, id: Option<&str>) -> usize {
    id.and_then(|id| cat.grupos.iter().position(|g| g.id == id))
        .unwrap_or(0)
}

/// The shipped blocks with made-up values, one change held: what `--dump`
/// draws, so a picture of the panel does not depend on the machine.
fn sample(grupo: Option<&str>) -> Panel {
    let cat = Catalogo::de_serie();
    let actual = cat.claves.iter().map(|c| c.normaliza(None)).collect();
    let mut p = Panel::new(cat, actual, Lang::current());
    // `dispositivos/instalar`: the page with the "Instalar driver" dialog open.
    let instalar = grupo.is_some_and(|g| g.ends_with("/instalar"));
    let grupo = grupo.map(|g| g.trim_end_matches("/instalar"));
    p.grupo = group_index(&p.cat, grupo);
    if let Some(i) = p.cat.claves.iter().position(|c| c.id == "teclado.idioma") {
        p.elige(i, "no".into());
    }
    if p.pagina_disp().is_some() {
        disp::muestra(&mut p);
        if instalar {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            let home = std::path::Path::new(&home);
            p.disp.instala = Some(disp::Instala {
                fila: p.disp.foco,
                candidatos: vec![
                    home.join("Descargas/mt7922-14c3-0616.drv"),
                    home.join("prueba.drv"),
                ],
                elegido: 0,
            });
        }
    }
    p
}

fn dump(spec: &str, grupo: Option<&str>) -> ! {
    let (path, dims) = spec.rsplit_once(':').unwrap_or((spec, "1600x900"));
    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((1600, 900));
    if fill_guard::check_buffer(w, h).is_err() {
        std::process::exit(1);
    }
    let p = sample(grupo);
    let mut cv = Canvas::try_new(w, h).unwrap_or_else(|| std::process::exit(1));
    cv.set_square(!glass::on());
    draw(&mut cv, w as i32, h as i32, &p);
    let mut buf = vec![0u8; w * h * 4];
    glass::finish(&mut cv);
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        eprintln!("lunarpanel: could not write {path}");
        std::process::exit(1);
    }
    println!("lunarpanel: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

/// Super+I twice closes it: a second start signals the first and leaves.
/// With `takeover` (a driver opened from the file browser) the second one
/// stays instead, once the first has gone.
fn toggle_or_lock(takeover: bool) -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&dir).join("lunarpanel.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .ok()?;
    // SAFETY: flock on a descriptor this process owns.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            // Only a process that is this program (see lunarnotes).
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if pid > 1 && comm.trim() == "lunarpanel" {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        let gone = takeover
            && (0..40).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                // SAFETY: as above.
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
            });
        if !gone {
            std::process::exit(0);
        }
    }
    let _ = std::fs::write(&path, std::process::id().to_string());
    Some(f)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|k| args.get(k + 1))
            .cloned()
    };
    match args.first().map(String::as_str) {
        None | Some("--grupo") | Some("--instalar") => {}
        Some("--dump") => dump(
            &args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "lunarpanel.raw".into()),
            args.get(2).map(String::as_str),
        ),
        Some(_) => {
            eprintln!(
                "usage: lunarpanel [--grupo ID] | --instalar FILE.drv | --dump PATH:WxH [GRUPO[/instalar]]"
            );
            std::process::exit(2);
        }
    }
    let instalar = arg("--instalar").map(std::path::PathBuf::from);
    let lock = toggle_or_lock(instalar.is_some());
    let dirs: Vec<&std::path::Path> = claves::DIRS.iter().map(std::path::Path::new).collect();
    let cat = Catalogo::carga(&dirs);
    for a in &cat.avisos {
        eprintln!("lunarpanel: {a}");
    }
    let actual = cat.claves.iter().map(claves::valor_actual).collect();
    let mut panel = Panel::new(cat, actual, Lang::current());
    panel.grupo = group_index(&panel.cat, arg("--grupo").as_deref());
    panel.carga_disp();
    if let Some(path) = instalar {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        panel.instala_fichero(path);
    }
    let mut st = State::new(panel);

    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarpanel: no Wayland compositor found");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarpanel: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    let (Some(comp), Some(ls)) = (st.compositor.clone(), st.layer_shell.clone()) else {
        eprintln!("lunarpanel: compositor lacks wlr-layer-shell");
        std::process::exit(1);
    };
    let surface = comp.create_surface(&qh, ());
    let layer = ls.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "settings".into(),
        &qh,
        (),
    );
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();
    st.surface = Some(surface);
    st.layer = Some(layer);
    while !st.done {
        if queue.blocking_dispatch(&mut st).is_err() {
            break;
        }
    }
    if let Some(l) = st.layer.take() {
        l.destroy();
    }
    if let Some(s) = st.surface.take() {
        s.destroy();
    }
    let _ = conn.flush();
    if st.reabrir {
        // Drawn again in the look or language just chosen, on the same page.
        drop(lock);
        use std::os::unix::process::CommandExt;
        let page = st
            .panel
            .cat
            .grupos
            .get(st.panel.grupo)
            .map(|g| g.id.clone())
            .unwrap_or_default();
        // The real path, not /proc/self/exe: the process name is what the
        // Super+I toggle recognises, and exec'ing the link would name it `exe`.
        let exe = std::env::current_exe().unwrap_or_else(|_| "lunarpanel".into());
        let err = std::process::Command::new(exe)
            .args(["--grupo", &page])
            .exec();
        eprintln!("lunarpanel: could not open again: {err}");
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "lunarpanel/tests.rs"]
mod tests;
