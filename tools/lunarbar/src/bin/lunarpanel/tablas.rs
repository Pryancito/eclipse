//! The "Servicios" and "Aperturas" pages: a group whose block is of type
//! `servicios` or `aperturas` draws a table instead of rows of settings.
//! What is in them comes from `lunarbar::servicios` and
//! `lunarbar::aperturas`; the columns, chips and buttons are computed here,
//! like the rest of the panel, from the texts they hold.
//!
//! A change (a service on or off at boot, another program for a kind of
//! file) is held in gold until Enter or APLICAR, and Esc drops it, as on
//! every other page.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use lunarbar::aperturas::{self, Apertura};
use lunarbar::claves::Valor;
use lunarbar::files::{kind_of, Kind};
use lunarbar::open;
use lunarbar::preview::kind_label;
use lunarbar::servicios::{self, Estado, Servicio};
use lunarbar::sysmon::{self, Verb};

/// One row: a name and the line under it.
const TROW_H: i32 = 40;
/// The column titles.
const HEAD_ROW_H: i32 = 24;
/// The focused row, said in three lines.
const DETAIL_H: i32 = 72;
const DOT: i32 = 8;
/// A chip in a row is narrower than a setting's: the text and a little air.
const TCHIP_PAD: i32 = 16;

/// What both pages keep of a list: the filter chosen, the focus among the
/// rows it leaves, and the first of them in view.
#[derive(Default)]
pub(super) struct Vista {
    pub filtro: usize,
    pub foco: usize,
    pub top: usize,
}

impl Vista {
    fn mueve(&mut self, d: i32, n: usize, caben: usize) {
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

    fn filtra(&mut self, f: usize) {
        *self = Vista {
            filtro: f,
            ..Vista::default()
        };
    }

    fn ajusta(&mut self, n: usize) {
        self.foco = self.foco.min(n.saturating_sub(1));
        self.top = self.top.min(self.foco);
    }
}

// ── the services ─────────────────────────────────────────────────────────────

/// The services page's state.
#[derive(Default)]
pub(super) struct Serv {
    pub filas: Vec<Servicio>,
    pub leido: bool,
    pub v: Vista,
    /// "Al arrancar" chosen and not applied yet, by name.
    pub pendiente: BTreeMap<String, bool>,
    /// The wall-clock second the rows were read at.
    pub ahora: u64,
    pub dir: PathBuf,
}

const SERV_FILTROS: [Key; 4] = [
    Key::hunt_all,
    Key::panel_svc_desktop,
    Key::panel_svc_drivers,
    Key::panel_svc_stopped_filter,
];

impl Serv {
    /// The rows the filter leaves, as indices into `filas`.
    pub fn visibles(&self) -> Vec<usize> {
        (0..self.filas.len())
            .filter(|&i| {
                let s = &self.filas[i];
                match self.v.filtro {
                    1 => s.escritorio,
                    2 => s.udriver.is_some(),
                    3 => s.estado() != Estado::EnMarcha,
                    _ => true,
                }
            })
            .collect()
    }

    fn enfocado(&self) -> Option<&Servicio> {
        self.visibles().get(self.v.foco).map(|&i| &self.filas[i])
    }

    /// Whether `s` starts at boot, with the change held.
    fn al_arrancar(&self, s: &Servicio) -> bool {
        self.pendiente
            .get(&s.nombre)
            .copied()
            .unwrap_or(s.al_arrancar)
    }

    /// Holds `on` for `s`, or drops the hold when that is how it is.
    pub(super) fn elige(&mut self, nombre: &str, on: bool) {
        let Some(s) = self.filas.iter().find(|s| s.nombre == nombre) else {
            return;
        };
        if s.esencial && !on {
            return;
        }
        if s.al_arrancar == on {
            self.pendiente.remove(nombre);
        } else {
            self.pendiente.insert(nombre.to_string(), on);
        }
    }
}

/// What init could be asked to do with `s` now: stop a running one, start
/// a stopped one. One it has not loaded it knows nothing of.
fn verbo(s: &Servicio) -> Option<Verb> {
    match s.estado() {
        Estado::EnMarcha | Estado::Esperando => Some(Verb::Stop),
        Estado::Parado | Estado::Abandonado => Some(Verb::Start),
        Estado::NoCargado => None,
    }
}

fn estado_serv(s: &Servicio, lang: Lang) -> (&'static str, Rgb) {
    match s.estado() {
        Estado::EnMarcha => (lang.s(Key::mon_running), ink().ok),
        Estado::Esperando => (lang.s(Key::panel_svc_waiting), ink().cyan),
        Estado::Parado if s.tipo == "oneshot" => (lang.s(Key::panel_svc_done), ink().dim),
        Estado::Parado => (lang.s(Key::panel_svc_stopped), ink().amber),
        Estado::Abandonado => (lang.s(Key::panel_svc_given_up), ink().warn),
        Estado::NoCargado if !s.al_arrancar => (lang.s(Key::hunt_sched_disabled), ink().dim),
        Estado::NoCargado => (lang.s(Key::panel_svc_not_loaded), ink().dim),
    }
}

fn por_que_serv(s: &Servicio, ahora: u64, lang: Lang) -> String {
    let started = s.ahora.as_ref().and_then(|x| x.started);
    let key = match s.estado() {
        Estado::EnMarcha => {
            return lang
                .s(Key::panel_svc_why_running)
                .replace("{}", &servicios::desde(started, ahora));
        }
        Estado::Esperando => Key::panel_svc_why_waiting,
        Estado::Parado if s.tipo == "oneshot" => Key::panel_svc_why_done,
        Estado::Parado => Key::panel_svc_why_stopped,
        Estado::Abandonado => Key::panel_svc_why_given_up,
        Estado::NoCargado if !s.al_arrancar => Key::panel_svc_why_off,
        Estado::NoCargado => Key::panel_svc_why_new,
    };
    lang.s(key).to_string()
}

// ── the openers ──────────────────────────────────────────────────────────────

/// The openers page's state.
#[derive(Default)]
pub(super) struct Aper {
    pub filas: Vec<Apertura>,
    pub leido: bool,
    pub v: Vista,
    /// The program chosen and not applied yet, by extension; `None` is as
    /// the image does.
    pub pendiente: BTreeMap<String, Option<String>>,
    /// The programs offered: the block's values that the image has.
    pub programas: Vec<Valor>,
    pub fichero: PathBuf,
}

const APER_FILTROS: [Key; 5] = [
    Key::hunt_all,
    Key::file_kind_document,
    Key::file_kind_code,
    Key::file_kind_image,
    Key::file_kind_media,
];

fn tipo(ext: &str) -> Kind {
    kind_of(&format!("x.{ext}"))
}

impl Aper {
    pub fn visibles(&self) -> Vec<usize> {
        (0..self.filas.len())
            .filter(|&i| {
                let k = tipo(&self.filas[i].ext);
                match self.v.filtro {
                    1 => !matches!(k, Kind::Code | Kind::Image | Kind::Media),
                    2 => k == Kind::Code,
                    3 => k == Kind::Image,
                    4 => k == Kind::Media,
                    _ => true,
                }
            })
            .collect()
    }

    fn enfocada(&self) -> Option<&Apertura> {
        self.visibles().get(self.v.foco).map(|&i| &self.filas[i])
    }

    /// The program that opens `a`, with the change held.
    fn programa(&self, a: &Apertura) -> Option<String> {
        match self.pendiente.get(&a.ext) {
            Some(Some(p)) => Some(p.clone()),
            Some(None) => a
                .de_serie
                .as_deref()
                .and_then(open::program_of)
                .map(Into::into),
            None => a.programa().map(Into::into),
        }
    }

    /// The command that opens `a`, with the change held.
    fn orden(&self, a: &Apertura) -> Option<String> {
        match self.pendiente.get(&a.ext) {
            Some(Some(p)) => Some(aperturas::orden_de(p)),
            Some(None) => a.de_serie.clone(),
            None => a.orden().map(Into::into),
        }
    }

    /// Whether `a` is opened by a program the page does not offer.
    fn fijo(&self, a: &Apertura) -> bool {
        let p = self.programa(a);
        !self.programas.iter().any(|v| Some(&v.v) == p.as_ref())
    }

    /// Holds `programa` for `a` (`None`: as the image does), or drops the
    /// hold when that is what is in force.
    pub(super) fn elige(&mut self, ext: &str, programa: Option<String>) {
        let Some(a) = self.filas.iter().find(|a| a.ext == ext) else {
            return;
        };
        let igual = match &programa {
            Some(p) => a.programa() == Some(p.as_str()),
            None => a.tuyo.is_none(),
        };
        if igual {
            self.pendiente.remove(ext);
        } else {
            self.pendiente.insert(ext.to_string(), programa);
        }
    }

    /// The next (or previous) offered program for `a`.
    pub(super) fn paso(&mut self, ext: &str, dir: i32) {
        let Some(a) = self.filas.iter().find(|a| a.ext == ext) else {
            return;
        };
        if self.fijo(a) || self.programas.is_empty() {
            return;
        }
        let n = self.programas.len() as i32;
        let p = self.programa(a);
        let at = self
            .programas
            .iter()
            .position(|v| Some(&v.v) == p.as_ref())
            .unwrap_or(0) as i32;
        let v = self.programas[((at + dir) % n + n) as usize % n as usize]
            .v
            .clone();
        self.elige(ext, Some(v));
    }
}

fn origen(p: &Aper, a: &Apertura, lang: Lang) -> (&'static str, Rgb) {
    if p.pendiente.contains_key(&a.ext) {
        (lang.s(Key::net_unapplied), ink().gold)
    } else if p.fijo(a) {
        (lang.s(Key::panel_ap_fixed), ink().dim)
    } else if a.cambiada() || a.de_serie.is_none() {
        (lang.s(Key::panel_ap_yours), ink().gold)
    } else {
        (lang.s(Key::panel_ap_builtin), ink().dim)
    }
}

/// The label of program `prog`: the block's word for it, else the program.
fn etiqueta(p: &Aper, prog: &str, en: bool) -> String {
    match p.programas.iter().find(|v| v.v == prog) {
        Some(v) if en => v.en.to_lowercase(),
        Some(v) => v.es.to_lowercase(),
        None => prog.to_string(),
    }
}

// ── reading ──────────────────────────────────────────────────────────────────

fn ahora() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Panel {
    /// The block of type `t` of the page on screen, when it is that page.
    fn pagina_tipo(&self, t: Tipo) -> Option<usize> {
        if !self.busca.trim().is_empty() {
            return None;
        }
        self.cat
            .del_grupo(self.grupo)
            .into_iter()
            .find(|&i| self.cat.claves[i].tipo == t)
    }

    pub(super) fn pagina_serv(&self) -> Option<usize> {
        self.pagina_tipo(Tipo::Servicios)
    }

    pub(super) fn pagina_aper(&self) -> Option<usize> {
        self.pagina_tipo(Tipo::Aperturas)
    }

    /// The block of type `t`, wherever it is.
    fn bloque(&self, t: Tipo) -> Option<usize> {
        self.cat.claves.iter().position(|c| c.tipo == t)
    }

    /// Reads the services and the openers, the ones not read yet: a
    /// directory and a small file, so the page list can count them.
    pub(super) fn carga_tablas(&mut self) {
        if let Some(i) = self.bloque(Tipo::Servicios).filter(|_| !self.serv.leido) {
            let c = &self.cat.claves[i];
            let dir = c
                .fichero
                .clone()
                .unwrap_or_else(|| "/etc/eclipse/services".into());
            let esenciales: Vec<&str> = c.valores.iter().map(|v| v.v.as_str()).collect();
            let svcs = std::fs::read_to_string(sysmon::SERVICES_FILE)
                .map(|t| sysmon::parse_services(&t))
                .unwrap_or_default();
            self.serv.filas = servicios::lee(&dir, &svcs, &esenciales);
            self.serv.dir = dir;
            self.serv.ahora = ahora();
            self.serv.leido = true;
            let n = self.serv.visibles().len();
            self.serv.v.ajusta(n);
        }
        if let Some(i) = self.bloque(Tipo::Aperturas).filter(|_| !self.aper.leido) {
            let c = &self.cat.claves[i];
            let fichero = c
                .fichero
                .clone()
                .unwrap_or_else(|| "/etc/eclipse/openers".into());
            let text = std::fs::read_to_string(&fichero).unwrap_or_default();
            self.aper.filas = aperturas::lee(&open::parse_openers(&text));
            self.aper.programas = c
                .valores
                .iter()
                .filter(|v| lunarbar::apps::executable_exists(&v.v))
                .cloned()
                .collect();
            self.aper.fichero = fichero;
            self.aper.leido = true;
            let n = self.aper.visibles().len();
            self.aper.v.ajusta(n);
        }
    }

    /// The count a page shows in the list, for these two pages once read.
    pub(super) fn cuenta_tabla(&self, g: usize) -> Option<usize> {
        let ids = self.cat.del_grupo(g);
        let es = |t: Tipo| ids.iter().any(|&i| self.cat.claves[i].tipo == t);
        if self.serv.leido && es(Tipo::Servicios) {
            Some(self.serv.filas.len())
        } else if self.aper.leido && es(Tipo::Aperturas) {
            Some(self.aper.filas.len())
        } else {
            None
        }
    }

    /// Whether a page's change is held in the list's group `g`.
    pub(super) fn tabla_pendiente(&self, g: usize) -> bool {
        let ids = self.cat.del_grupo(g);
        let es = |t: Tipo| ids.iter().any(|&i| self.cat.claves[i].tipo == t);
        (es(Tipo::Servicios) && !self.serv.pendiente.is_empty())
            || (es(Tipo::Aperturas) && !self.aper.pendiente.is_empty())
    }
}

// ── layout ───────────────────────────────────────────────────────────────────

/// Where a table page's parts are.
pub(super) struct TL {
    /// The x of each column.
    pub cols: Vec<i32>,
    pub head_y: i32,
    pub filas: Vec<Rect>,
    pub caben: usize,
    pub detalle: Rect,
    /// The filter chips, on the title's line.
    pub filtros: Vec<Rect>,
    /// The foot's buttons, in the order given.
    pub botones: Vec<Rect>,
}

/// A foot button: its text and air. Four of them share the foot with the
/// counts, so not the other pages' wider minimum.
fn btn_w(label: &str) -> i32 {
    up4(Canvas::text_width(label) + 2 * PAD).max(BTN_W * 4 / 5)
}

fn chip_w(label: &str) -> i32 {
    up4(Canvas::text_width(label) + TCHIP_PAD)
}

/// A table of `n` rows from `top`, columns at `pct` of the width, filters
/// right of the title and buttons right in the foot.
fn layout_tabla(
    l: &Layout,
    n: usize,
    top: usize,
    pct: &[i32],
    filtros: &[&str],
    botones: &[&str],
) -> TL {
    let rx = l.body.0 + PAD + 4;
    let rw = l.body.2 - 2 * PAD - 8;
    let head_y = l.body.1 + PAD / 2 + NAV_ROW_H;
    let first_y = head_y + HEAD_ROW_H;
    let detalle = (rx - 4, l.body.1 + l.body.3 - DETAIL_H - 8, rw + 8, DETAIL_H);
    let caben = ((detalle.1 - 8 - first_y) / TROW_H).max(1) as usize;
    let filas = (0..n.saturating_sub(top).min(caben))
        .map(|k| (rx, first_y + k as i32 * TROW_H, rw, TROW_H))
        .collect();
    let fy = l.body.1 + PAD / 2 + (NAV_ROW_H - CTRL_H) / 2;
    let mut x = rx + rw;
    let mut fr: Vec<Rect> = filtros
        .iter()
        .rev()
        .map(|t| {
            let w = chip_w(t) + 8;
            x -= w;
            let r = (x, fy, w, CTRL_H);
            x -= CHIP_GAP;
            r
        })
        .collect();
    fr.reverse();
    let mut x = l.apply.0 + l.apply.2;
    let mut br: Vec<Rect> = botones
        .iter()
        .rev()
        .map(|t| {
            let w = btn_w(t);
            x -= w;
            let r = (x, l.apply.1, w, BTN_H);
            x -= 12;
            r
        })
        .collect();
    br.reverse();
    TL {
        cols: pct.iter().map(|p| rx + up4(rw * p / 100)).collect(),
        head_y,
        filas,
        caben,
        detalle,
        filtros: fr,
        botones: br,
    }
}

fn serv_botones(p: &Panel) -> [String; 4] {
    let l = p.lang;
    let accion = match p.serv.enfocado().and_then(verbo) {
        Some(Verb::Stop) => Key::mon_stop,
        _ => Key::mon_start,
    };
    [
        l.s(Key::panel_svc_log_caps).into(),
        l.s(Key::mon_restart).into(),
        l.s(accion).into(),
        l.s(Key::net_apply_caps).into(),
    ]
}

fn aper_botones(p: &Panel) -> [String; 2] {
    let l = p.lang;
    [
        l.s(Key::panel_ap_builtin_caps).into(),
        l.s(Key::net_apply_caps).into(),
    ]
}

pub(super) fn layout_serv(l: &Layout, p: &Panel) -> TL {
    let f: Vec<&str> = SERV_FILTROS.iter().map(|k| p.lang.s(*k)).collect();
    let b = serv_botones(p);
    let b: Vec<&str> = b.iter().map(String::as_str).collect();
    layout_tabla(
        l,
        p.serv.visibles().len(),
        p.serv.v.top,
        &[0, 34, 56, 80],
        &f,
        &b,
    )
}

pub(super) fn layout_aper(l: &Layout, p: &Panel) -> TL {
    let f: Vec<&str> = APER_FILTROS.iter().map(|k| p.lang.s(*k)).collect();
    let b = aper_botones(p);
    let b: Vec<&str> = b.iter().map(String::as_str).collect();
    layout_tabla(
        l,
        p.aper.visibles().len(),
        p.aper.v.top,
        &[0, 20, 84],
        &f,
        &b,
    )
}

/// The "sí" and "no" chips of a service row whose column starts at `x`.
fn chips_boot(x: i32, row: Rect, lang: Lang) -> [Rect; 2] {
    let y = row.1 + (TROW_H - CTRL_H) / 2;
    let w1 = chip_w(lang.s(Key::panel_on));
    let w2 = chip_w(lang.s(Key::hunt_sched_off));
    [(x, y, w1, CTRL_H), (x + w1 + CHIP_GAP, y, w2, CTRL_H)]
}

/// The program chips of an openers row whose column starts at `x`.
fn chips_prog(x: i32, row: Rect, p: &Aper, en: bool) -> Vec<Rect> {
    let y = row.1 + (TROW_H - CTRL_H) / 2;
    let mut x = x;
    p.programas
        .iter()
        .map(|v| {
            let w = chip_w(&etiqueta(p, &v.v, en));
            let r = (x, y, w, CTRL_H);
            x += w + CHIP_GAP;
            r
        })
        .collect()
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// What both pages draw round their rows: the title and its filters, the
/// column titles, the "x–y / n" when the rows do not all fit, the detail
/// box and the foot's rule and buttons.
#[allow(clippy::too_many_arguments)]
fn draw_marco(
    cv: &mut Canvas,
    l: &Layout,
    t: &TL,
    title: &str,
    filtros: &[Key],
    filtro: usize,
    heads: &[&str],
    lang: Lang,
) {
    let rx = t.cols[0];
    let rw = l.body.2 - 2 * PAD - 8;
    let sec_y = l.body.1 + PAD / 2 + (NAV_ROW_H - GLYPH_H) / 2;
    let stw = cv.text_bold(title, rx, sec_y, ink().text);
    let end = t.filtros.first().map_or(rx + rw, |r| r.0 - 12);
    if end > rx + stw + 24 {
        cv.hline(
            rx + stw + 12,
            sec_y + GLYPH_H / 2,
            end - rx - stw - 24,
            ink().hairline,
            1.0,
        );
    }
    for (k, (r, key)) in t.filtros.iter().zip(filtros).enumerate() {
        chip(cv, *r, lang.s(*key), k == filtro, false);
    }
    for (x, h) in t.cols.iter().zip(heads) {
        cv.text(h, *x, t.head_y + (HEAD_ROW_H - GLYPH_H) / 2 - 2, ink().dim);
    }
    cv.hline(rx, t.head_y + HEAD_ROW_H - 2, rw, ink().hairline, 1.0);
    let dr = t.detalle;
    glass::cell(cv, dr.0, dr.1, dr.2, dr.3, ink().well, 1.0);
    frame(cv, dr, ink().hairline);
    let (px, py, pw, ph) = l.panel;
    cv.hline(
        px + PAD,
        py + ph - FOOT_H,
        pw - 2 * PAD,
        ink().hairline,
        1.0,
    );
}

fn draw_fila_marco(cv: &mut Canvas, r: Rect, focused: bool) {
    let (x, y, fw, fh) = r;
    if focused {
        glass::cell(cv, x - 6, y + 1, fw + 12, fh - 2, ink().cyan, 0.08);
        glass::brackets(cv, x - 8, y - 1, fw + 16, fh + 2, 8, 2, ink().gold);
    }
    cv.hline(x, y + fh - 1, fw, ink().hairline, 0.5);
}

/// "3–11 / 27" above the detail box, when not every row fits.
fn draw_cuantas(cv: &mut Canvas, t: &TL, top: usize, n: usize) {
    let shown = t.filas.len();
    if top > 0 || top + shown < n {
        let s = format!("{}–{} / {}", top + 1, top + shown, n);
        let rx = t.detalle.0 + 4;
        let rw = t.detalle.2 - 8;
        cv.text(
            &s,
            rx + rw - Canvas::text_width(&s),
            t.detalle.1 - GLYPH_H - 6,
            ink().dim,
        );
    }
}

/// The detail box's three lines.
fn draw_detalle(cv: &mut Canvas, t: &TL, lines: [(&str, Rgb); 3]) {
    let dr = t.detalle;
    let room = dr.2 - 2 * PAD;
    for (k, (s, c)) in lines.iter().enumerate() {
        let y = dr.1 + 10 + k as i32 * (GLYPH_H + 5);
        if k == 0 {
            cv.text_bold(&fit(s, room), dr.0 + PAD, y, *c);
        } else {
            cv.text(&fit(s, room), dr.0 + PAD, y, *c);
        }
    }
}

/// The foot's left side: the status when there is one, else the counts.
fn draw_pie(cv: &mut Canvas, l: &Layout, t: &TL, p: &Panel, cuentas: &[(usize, &str, Rgb)]) {
    let (px, py, _, ph) = l.panel;
    let fy = py + ph - FOOT_H + (FOOT_H - GLYPH_H) / 2;
    let room = t.botones.first().map_or(l.apply.0, |b| b.0) - px - 2 * PAD;
    if !p.status.is_empty() {
        let col = match p.tono {
            Tono::Plano => ink().muted,
            Tono::Bien => ink().cyan,
            Tono::Mal => ink().warn,
        };
        cv.text(&fit(&p.status, room), px + PAD, fy, col);
        return;
    }
    let mut x = px + PAD;
    for (k, (num, word, col)) in cuentas.iter().enumerate() {
        // The first count is always said; the others only when not zero.
        if *num == 0 && k > 0 {
            continue;
        }
        let s = format!("{num} {word}");
        if x + Canvas::text_width(&s) > px + PAD + room {
            break;
        }
        x += cv.text(&num.to_string(), x, fy, *col);
        x += cv.text(&format!(" {word}  "), x, fy, ink().muted);
    }
}

pub(super) fn draw_serv(cv: &mut Canvas, l: &Layout, p: &Panel, ci: usize) {
    let en = p.lang == Lang::En;
    let lang = p.lang;
    let s = &p.serv;
    let t = layout_serv(l, p);
    let vis = s.visibles();
    let title = p.cat.claves[ci].nombre(en).to_uppercase();
    let heads = [
        lang.s(Key::mon_service),
        lang.s(Key::mon_state),
        lang.s(Key::panel_svc_col_boot),
        lang.s(Key::mon_for),
    ];
    draw_marco(cv, l, &t, &title, &SERV_FILTROS, s.v.filtro, &heads, lang);
    if s.filas.is_empty() {
        let none = lang
            .s(Key::panel_svc_none)
            .replace("{}", &s.dir.display().to_string());
        cv.text(&none, t.cols[0], t.head_y + HEAD_ROW_H + 12, ink().muted);
    }
    for (k, r) in t.filas.iter().enumerate() {
        let i = vis[s.v.top + k];
        let sv = &s.filas[i];
        let focused = s.v.top + k == s.v.foco;
        draw_fila_marco(cv, *r, focused);
        let (x, y, fw, fh) = *r;
        let name_w = t.cols[1] - x - PAD;
        cv.text(&fit(&sv.nombre, name_w), x, y + 4, ink().text);
        cv.text(&fit(&sv.sub(), name_w), x, y + 4 + GLYPH_H + 2, ink().dim);
        let (word, col) = estado_serv(sv, lang);
        cv.fill_rect_a(t.cols[1], y + (fh - DOT) / 2, DOT, DOT, col, 1.0);
        let ww = t.cols[2] - t.cols[1] - DOT - 8 - PAD;
        cv.text(
            &fit(word, ww),
            t.cols[1] + DOT + 8,
            y + (fh - GLYPH_H) / 2,
            col,
        );
        if sv.esencial {
            let e = format!(
                "{} · {}",
                lang.s(Key::panel_on),
                lang.s(Key::panel_svc_essential)
            );
            cv.text(
                &fit(&e, t.cols[3] - t.cols[2] - PAD),
                t.cols[2],
                y + (fh - GLYPH_H) / 2,
                ink().dim,
            );
        } else {
            let on = s.al_arrancar(sv);
            let held = s.pendiente.contains_key(&sv.nombre);
            let [si, no] = chips_boot(t.cols[2], *r, lang);
            chip(cv, si, lang.s(Key::panel_on), on, held && on);
            chip(cv, no, lang.s(Key::hunt_sched_off), !on, held && !on);
        }
        let started = sv.ahora.as_ref().and_then(|x| x.started);
        let desde = match sv.estado() {
            Estado::EnMarcha => servicios::desde(started, s.ahora),
            Estado::Abandonado => {
                let n = sv.ahora.as_ref().map_or(0, |x| x.starts);
                lang.s(Key::panel_svc_retries).replace("{}", &n.to_string())
            }
            _ => "—".into(),
        };
        cv.text(
            &fit(&desde, x + fw - t.cols[3]),
            t.cols[3],
            y + (fh - GLYPH_H) / 2,
            ink().muted,
        );
    }
    draw_cuantas(cv, &t, s.v.top, vis.len());

    if let Some(sv) = s.enfocado() {
        let head = if sv.exec.is_empty() {
            sv.nombre.clone()
        } else {
            format!("{} · {}", sv.nombre, sv.exec)
        };
        let boot = if s.al_arrancar(sv) {
            lang.s(Key::panel_svc_boots).to_string()
        } else {
            lang.s(Key::panel_svc_why_off).to_string()
        };
        let mut why = por_que_serv(sv, s.ahora, lang);
        if why != boot {
            why = format!("{why}. {boot}.");
        }
        why += &format!(" {} {}", lang.s(Key::panel_svc_log), sv.registro());
        let hint = if sv.esencial {
            lang.s(Key::panel_svc_essential_hint).to_string()
        } else {
            lang.s(Key::panel_svc_off_hint).replace("{}", &sv.nombre)
        };
        draw_detalle(
            cv,
            &t,
            [(&head, ink().gold), (&why, ink().muted), (&hint, ink().dim)],
        );
    }

    let labels = serv_botones(p);
    let held = !s.pendiente.is_empty();
    let accion = s.enfocado().and_then(verbo).is_some();
    for (k, (r, label)) in t.botones.iter().zip(&labels).enumerate() {
        let lit = match k {
            2 => accion && !held,
            3 => held,
            _ => false,
        };
        button(cv, *r, label, lit);
    }
    let count = |e: Estado| s.filas.iter().filter(|x| x.estado() == e).count();
    draw_pie(
        cv,
        l,
        &t,
        p,
        &[
            (count(Estado::EnMarcha), lang.s(Key::mon_running), ink().ok),
            (
                count(Estado::Parado),
                lang.s(Key::panel_svc_n_stopped),
                ink().amber,
            ),
            (
                count(Estado::Abandonado),
                lang.s(Key::panel_svc_n_given_up),
                ink().warn,
            ),
            (s.pendiente.len(), lang.s(Key::net_unapplied), ink().gold),
        ],
    );
}

pub(super) fn draw_aper(cv: &mut Canvas, l: &Layout, p: &Panel, ci: usize) {
    let en = p.lang == Lang::En;
    let lang = p.lang;
    let a = &p.aper;
    let t = layout_aper(l, p);
    let vis = a.visibles();
    let title = p.cat.claves[ci].nombre(en).to_uppercase();
    let heads = [
        lang.s(Key::mon_kind),
        lang.s(Key::panel_ap_col_program),
        lang.s(Key::panel_ap_col_origin),
    ];
    draw_marco(cv, l, &t, &title, &APER_FILTROS, a.v.filtro, &heads, lang);
    for (k, r) in t.filas.iter().enumerate() {
        let ap = &a.filas[vis[a.v.top + k]];
        let focused = a.v.top + k == a.v.foco;
        draw_fila_marco(cv, *r, focused);
        let (x, y, fw, fh) = *r;
        let name_w = t.cols[1] - x - PAD;
        cv.text_bold(&fit(&format!(".{}", ap.ext), name_w), x, y + 4, ink().text);
        let kind = kind_label(tipo(&ap.ext), false, lang);
        cv.text(&fit(kind, name_w), x, y + 4 + GLYPH_H + 2, ink().dim);
        let room = t.cols[2] - t.cols[1] - PAD;
        if a.fijo(ap) {
            let o = a.orden(ap).unwrap_or_else(|| "—".into());
            cv.text(&fit(&o, room), t.cols[1], y + (fh - GLYPH_H) / 2, ink().dim);
        } else {
            let prog = a.programa(ap);
            let held = a.pendiente.contains_key(&ap.ext);
            for (r, v) in chips_prog(t.cols[1], *r, a, en).iter().zip(&a.programas) {
                if r.0 + r.2 > t.cols[1] + room {
                    break;
                }
                let on = prog.as_deref() == Some(v.v.as_str());
                chip(cv, *r, &etiqueta(a, &v.v, en), on, held && on);
            }
        }
        let (word, col) = origen(a, ap, lang);
        cv.text(
            &fit(word, x + fw - t.cols[2]),
            t.cols[2],
            y + (fh - GLYPH_H) / 2,
            col,
        );
    }
    draw_cuantas(cv, &t, a.v.top, vis.len());

    if let Some(ap) = a.enfocada() {
        let head = format!(
            ".{} · {}",
            ap.ext,
            a.orden(ap).unwrap_or_else(|| "—".into())
        );
        let f = a.fichero.display().to_string();
        let de_serie = ap
            .de_serie
            .as_deref()
            .and_then(open::program_of)
            .map(|q| etiqueta(a, q, en));
        let why = if a.pendiente.contains_key(&ap.ext) {
            lang.s(Key::panel_ap_why_held).replace("{}", &f)
        } else if a.fijo(ap) {
            lang.s(Key::panel_ap_why_fixed).to_string()
        } else if ap.de_serie.is_none() {
            lang.s(Key::panel_ap_why_own).replace("{}", &f)
        } else if ap.cambiada() {
            lang.s(Key::panel_ap_why_yours)
                .replacen("{}", de_serie.as_deref().unwrap_or("—"), 1)
                .replacen("{}", &f, 1)
        } else {
            lang.s(Key::panel_ap_why_builtin)
                .replace("{}", de_serie.as_deref().unwrap_or("—"))
        };
        draw_detalle(
            cv,
            &t,
            [
                (&head, ink().gold),
                (&why, ink().muted),
                (lang.s(Key::panel_ap_only), ink().dim),
            ],
        );
    }

    let labels = aper_botones(p);
    let held = !a.pendiente.is_empty();
    for (k, (r, label)) in t.botones.iter().zip(&labels).enumerate() {
        button(cv, *r, label, k == 1 && held);
    }
    let cambiadas = a
        .filas
        .iter()
        .filter(|x| x.cambiada() || x.de_serie.is_none())
        .count();
    draw_pie(
        cv,
        l,
        &t,
        p,
        &[
            (a.filas.len(), lang.s(Key::panel_ap_kinds), ink().cyan),
            (cambiadas, lang.s(Key::panel_ap_changed), ink().cyan),
            (a.pendiente.len(), lang.s(Key::net_unapplied), ink().gold),
        ],
    );
}

// ── what the pages do ────────────────────────────────────────────────────────

impl State {
    fn bien(&mut self, s: String) {
        self.panel.status = s;
        self.panel.tono = Tono::Bien;
    }

    fn mal(&mut self, s: String) {
        self.panel.status = s;
        self.panel.tono = Tono::Mal;
    }

    /// Every held "al arrancar", renamed in the services directory.
    fn serv_aplica(&mut self) {
        let s = &self.panel.serv;
        let mut hechos = 0;
        let mut fallo = None;
        for (nombre, on) in &s.pendiente {
            let Some(sv) = s.filas.iter().find(|x| x.nombre == *nombre) else {
                continue;
            };
            match servicios::al_arrancar(&s.dir, sv, *on) {
                Ok(()) => hechos += 1,
                Err(e) => {
                    eprintln!("lunarpanel: {e}");
                    fallo.get_or_insert(e);
                }
            }
        }
        self.panel.serv.pendiente.clear();
        self.panel.serv.leido = false;
        self.panel.carga_tablas();
        let lang = self.panel.lang;
        match fallo {
            Some(e) => self.mal(e),
            None => self.bien(format!(
                "{} · {hechos} · {}",
                lang.s(Key::hunt_applied),
                lang.s(Key::panel_on_reboot)
            )),
        }
    }

    /// Asks init to start, stop or restart the focused service.
    fn serv_pide(&mut self, verb: Option<Verb>) {
        let Some(sv) = self.panel.serv.enfocado() else {
            return;
        };
        let lang = self.panel.lang;
        let nombre = sv.nombre.clone();
        let Some(verb) = verb.filter(|_| sv.estado() != Estado::NoCargado) else {
            self.mal(format!("{nombre}: {}", lang.s(Key::panel_svc_why_new)));
            return;
        };
        match sysmon::request(verb, &nombre) {
            Ok(()) => {
                let what = lang.s(match verb {
                    Verb::Start => Key::mon_start,
                    Verb::Stop => Key::mon_stop,
                    Verb::Restart => Key::mon_restart,
                });
                self.bien(format!(
                    "{} {} {nombre}",
                    lang.s(Key::panel_svc_asked),
                    what.to_lowercase()
                ));
                self.panel.serv.leido = false;
                self.panel.carga_tablas();
            }
            Err(e) => self.mal(format!("{nombre}: {} ({e})", lang.s(Key::mon_could_not))),
        }
    }

    /// The focused service's log, in the editor.
    fn serv_registro(&mut self) {
        let Some(sv) = self.panel.serv.enfocado() else {
            return;
        };
        let log = sv.registro();
        if !std::path::Path::new(&log).is_file() {
            let no = self.panel.lang.s(Key::mon_this_service_keeps_no_log);
            self.mal(format!("{} · {log}", no));
            return;
        }
        let ok = std::process::Command::new("eclipse-edit")
            .arg(&log)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok();
        if ok {
            self.panel.status = log;
            self.panel.tono = Tono::Plano;
        } else {
            self.mal(format!("eclipse-edit {log}"));
        }
    }

    /// Flips the focused service's "al arrancar", held.
    fn serv_cambia(&mut self, on: Option<bool>) {
        let Some(sv) = self.panel.serv.enfocado() else {
            return;
        };
        let nombre = sv.nombre.clone();
        let on = on.unwrap_or(!self.panel.serv.al_arrancar(sv));
        self.panel.serv.elige(&nombre, on);
        self.panel.status.clear();
    }

    pub(super) fn serv_key(&mut self, key: u32) -> bool {
        let l = self.layout();
        let caben = layout_serv(&l, &self.panel).caben;
        let n = self.panel.serv.visibles().len();
        let s = &mut self.panel.serv;
        match key {
            KEY_UP_WL => s.v.mueve(-1, n, caben),
            KEY_DOWN_WL => s.v.mueve(1, n, caben),
            KEY_PGUP_WL => s.v.mueve(-(caben as i32), n, caben),
            KEY_PGDN_WL => s.v.mueve(caben as i32, n, caben),
            KEY_LEFT_WL | KEY_RIGHT_WL | KEY_SPACE => {
                self.serv_cambia(None);
                return true;
            }
            KEY_ENTER_WL | KEY_KPENTER_WL => {
                if s.pendiente.is_empty() {
                    let v = s.enfocado().and_then(verbo);
                    self.serv_pide(v);
                } else {
                    self.serv_aplica();
                }
                return true;
            }
            KEY_ESC_WL if !s.pendiente.is_empty() => {
                s.pendiente.clear();
                self.panel.status = self.panel.lang.s(Key::panel_undone).into();
                self.panel.tono = Tono::Plano;
                return true;
            }
            _ => return false,
        }
        self.panel.status.clear();
        true
    }

    pub(super) fn serv_click(&mut self, x: i32, y: i32) -> bool {
        let l = self.layout();
        if l.nav.iter().any(|r| hit(*r, x, y)) || hit(l.search, x, y) {
            return false;
        }
        let t = layout_serv(&l, &self.panel);
        if let Some(k) = t.filtros.iter().position(|r| hit(*r, x, y)) {
            self.panel.serv.v.filtra(k);
            self.panel.status.clear();
        } else if let Some(k) = t.botones.iter().position(|r| hit(*r, x, y)) {
            match k {
                0 => self.serv_registro(),
                1 => self.serv_pide(Some(Verb::Restart)),
                2 => {
                    let v = self.panel.serv.enfocado().and_then(verbo);
                    self.serv_pide(v);
                }
                _ => {
                    if !self.panel.serv.pendiente.is_empty() {
                        self.serv_aplica();
                    }
                }
            }
        } else if let Some(k) = t.filas.iter().position(|r| hit(*r, x, y)) {
            self.panel.serv.v.foco = self.panel.serv.v.top + k;
            self.panel.status.clear();
            let [si, no] = chips_boot(t.cols[2], t.filas[k], self.panel.lang);
            if hit(si, x, y) {
                self.serv_cambia(Some(true));
            } else if hit(no, x, y) {
                self.serv_cambia(Some(false));
            }
        }
        true
    }

    /// Every held program, written to the openers file.
    fn aper_aplica(&mut self) {
        let a = &self.panel.aper;
        let mut hechos = 0;
        let mut fallo = None;
        for (ext, prog) in &a.pendiente {
            let Some(ap) = a.filas.iter().find(|x| x.ext == *ext) else {
                continue;
            };
            match aperturas::cambia(&a.fichero, ap, prog.as_deref()) {
                Ok(()) => hechos += 1,
                Err(e) => {
                    eprintln!("lunarpanel: {e}");
                    fallo.get_or_insert(e);
                }
            }
        }
        self.panel.aper.pendiente.clear();
        self.panel.aper.leido = false;
        self.panel.carga_tablas();
        match fallo {
            Some(e) => self.mal(e),
            None => self.bien(format!(
                "{} · {hechos}",
                self.panel.lang.s(Key::hunt_applied)
            )),
        }
    }

    fn aper_de_serie(&mut self) {
        let Some(ap) = self.panel.aper.enfocada() else {
            return;
        };
        let ext = ap.ext.clone();
        self.panel.aper.elige(&ext, None);
        self.panel.status.clear();
    }

    fn aper_paso(&mut self, dir: i32) {
        let Some(ap) = self.panel.aper.enfocada() else {
            return;
        };
        let ext = ap.ext.clone();
        self.panel.aper.paso(&ext, dir);
        self.panel.status.clear();
    }

    pub(super) fn aper_key(&mut self, key: u32) -> bool {
        let l = self.layout();
        let caben = layout_aper(&l, &self.panel).caben;
        let n = self.panel.aper.visibles().len();
        let a = &mut self.panel.aper;
        match key {
            KEY_UP_WL => a.v.mueve(-1, n, caben),
            KEY_DOWN_WL => a.v.mueve(1, n, caben),
            KEY_PGUP_WL => a.v.mueve(-(caben as i32), n, caben),
            KEY_PGDN_WL => a.v.mueve(caben as i32, n, caben),
            KEY_LEFT_WL => {
                self.aper_paso(-1);
                return true;
            }
            KEY_RIGHT_WL | KEY_SPACE => {
                self.aper_paso(1);
                return true;
            }
            KEY_ENTER_WL | KEY_KPENTER_WL => {
                if !a.pendiente.is_empty() {
                    self.aper_aplica();
                }
                return true;
            }
            KEY_ESC_WL if !a.pendiente.is_empty() => {
                a.pendiente.clear();
                self.panel.status = self.panel.lang.s(Key::panel_undone).into();
                self.panel.tono = Tono::Plano;
                return true;
            }
            _ => return false,
        }
        self.panel.status.clear();
        true
    }

    pub(super) fn aper_click(&mut self, x: i32, y: i32) -> bool {
        let l = self.layout();
        if l.nav.iter().any(|r| hit(*r, x, y)) || hit(l.search, x, y) {
            return false;
        }
        let t = layout_aper(&l, &self.panel);
        let en = self.panel.lang == Lang::En;
        if let Some(k) = t.filtros.iter().position(|r| hit(*r, x, y)) {
            self.panel.aper.v.filtra(k);
            self.panel.status.clear();
        } else if hit(t.botones[0], x, y) {
            self.aper_de_serie();
        } else if hit(t.botones[1], x, y) {
            if !self.panel.aper.pendiente.is_empty() {
                self.aper_aplica();
            }
        } else if let Some(k) = t.filas.iter().position(|r| hit(*r, x, y)) {
            let a = &mut self.panel.aper;
            a.v.foco = a.v.top + k;
            self.panel.status.clear();
            let Some(ap) = a.enfocada() else { return true };
            if a.fijo(ap) {
                return true;
            }
            let ext = ap.ext.clone();
            let chips = chips_prog(t.cols[1], t.filas[k], a, en);
            if let Some(n) = chips.iter().position(|r| hit(*r, x, y)) {
                let v = a.programas[n].v.clone();
                a.elige(&ext, Some(v));
            }
        }
        true
    }
}

// ── `--dump` ─────────────────────────────────────────────────────────────────

/// Both pages' made-up rows, for `--dump`.
pub(super) fn muestra(p: &mut Panel) {
    if let Some(ci) = p.bloque(Tipo::Servicios) {
        muestra_serv(p, ci);
    }
    if let Some(ci) = p.bloque(Tipo::Aperturas) {
        muestra_aper(p, ci);
    }
}

/// A made-up machine for `--dump servicios`: the Abyss session up, the
/// antivirus stopped by hand, its scheduler off at boot, a driver init gave
/// up on, and ntpd held off.
fn muestra_serv(p: &mut Panel, ci: usize) {
    use lunarbar::sysmon::Svc;
    let ahora = 1_000_000;
    let esenciales: Vec<String> = p.cat.claves[ci]
        .valores
        .iter()
        .map(|v| v.v.clone())
        .collect();
    #[allow(clippy::type_complexity)]
    let filas: [(
        &str,
        &str,
        &str,
        Option<(&str, u64)>,
        bool,
        bool,
        Option<&str>,
    ); 13] = [
        (
            "abyss",
            "Abyss Wayland session",
            "respawn",
            Some(("running", 8040)),
            true,
            true,
            None,
        ),
        (
            "abyss-hyperion-bar",
            "hyperion-bar for the Abyss session",
            "respawn",
            Some(("running", 8030)),
            true,
            true,
            None,
        ),
        (
            "abyss-seatd",
            "Seat manager for the Abyss session",
            "respawn",
            Some(("running", 8050)),
            true,
            true,
            None,
        ),
        (
            "boot-sound",
            "Sonido de arranque",
            "oneshot",
            Some(("stopped", 0)),
            true,
            false,
            None,
        ),
        (
            "dbus",
            "Bus de sesión",
            "respawn",
            Some(("running", 8050)),
            true,
            false,
            None,
        ),
        (
            "hunter-scheduler",
            "Escaneos programados",
            "oneshot",
            None,
            false,
            false,
            None,
        ),
        (
            "hunterd",
            "Antivirus",
            "respawn",
            Some(("stopped", 0)),
            true,
            false,
            None,
        ),
        (
            "network",
            "Red",
            "oneshot",
            Some(("idle", 0)),
            true,
            false,
            None,
        ),
        (
            "ntpd",
            "Hora por red",
            "respawn",
            Some(("running", 7990)),
            true,
            false,
            None,
        ),
        (
            "oopslog",
            "Fallos del núcleo",
            "oneshot",
            Some(("stopped", 0)),
            true,
            false,
            None,
        ),
        (
            "uahci",
            "",
            "respawn",
            Some(("running", 8055)),
            true,
            false,
            Some("ahci@0"),
        ),
        (
            "udrv-14c3-0616",
            "",
            "respawn",
            Some(("given-up", 0)),
            true,
            false,
            Some("pci-14c3-0616@0"),
        ),
        (
            "unvme",
            "",
            "respawn",
            Some(("running", 8055)),
            true,
            false,
            Some("nvme@0"),
        ),
    ];
    p.serv.filas = filas
        .iter()
        .map(|(n, que, tipo, st, on, desk, drv)| Servicio {
            nombre: n.to_string(),
            que: que.to_string(),
            exec: format!("/usr/local/bin/{n}"),
            tipo: tipo.to_string(),
            escritorio: *desk,
            udriver: drv.map(Into::into),
            log: None,
            al_arrancar: *on,
            esencial: esenciales.iter().any(|e| e == n),
            ahora: st.map(|(state, secs)| Svc {
                name: n.to_string(),
                kind: tipo.to_string(),
                state: state.into(),
                started: (state == "running").then(|| ahora - secs),
                starts: if state == "given-up" { 5 } else { 1 },
                ..Default::default()
            }),
        })
        .collect();
    p.serv.ahora = ahora;
    p.serv.dir = "/etc/eclipse/services".into();
    p.serv.pendiente.insert("ntpd".into(), false);
    p.serv.v.foco = 6;
    p.serv.leido = true;
}

/// For `--dump aperturas`: the image's table, `.svg` changed to the viewer
/// in the file, `.xml` held for the editor, and every program offered.
fn muestra_aper(p: &mut Panel, ci: usize) {
    p.aper.filas = aperturas::lee(&open::parse_openers("svg=eclipse-view %f\n"));
    p.aper.programas = p.cat.claves[ci].valores.clone();
    p.aper.fichero = "/etc/eclipse/openers".into();
    p.aper
        .pendiente
        .insert("xml".into(), Some("eclipse-edit".into()));
    p.aper.v.foco = p
        .aper
        .filas
        .iter()
        .position(|a| a.ext == "svg")
        .unwrap_or(0);
    p.aper.v.top = p.aper.v.foco.saturating_sub(4);
    p.aper.leido = true;
}
