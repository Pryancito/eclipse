//! The "Dispositivos y drivers" page: a group whose block is of type
//! `dispositivos` draws the machine's PCI functions instead of rows of
//! settings. What is drawn comes from `lunarbar::dispositivos`; the
//! columns, rows and buttons are computed here like the rest of the panel.

use super::*;
use lunarbar::dispositivos::{self, Estado, Fila, Nombres};

/// One device row: name and the line under it.
const DEV_ROW_H: i32 = 40;
/// The column titles.
const HEAD_ROW_H: i32 = 24;
/// The selected device's two lines.
const DETAIL_H: i32 = 56;
const DOT: i32 = 8;

/// The page's state: the rows, read when the page is first shown.
#[derive(Default)]
pub(super) struct Disp {
    pub filas: Vec<Fila>,
    pub leido: bool,
    /// `pci.ids` was not there: names are ids.
    pub sin_nombres: bool,
    pub foco: usize,
    pub top: usize,
    /// The "Instalar driver" dialog, while it is open.
    pub instala: Option<Instala>,
}

/// The dialog's state: the device it is for and the files that could be
/// its driver, one of them chosen.
pub(super) struct Instala {
    pub fila: usize,
    pub candidatos: Vec<std::path::PathBuf>,
    pub elegido: usize,
}

/// `path` with the home directory written `~`.
fn corto(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

impl Disp {
    pub fn mueve(&mut self, d: i32, caben: usize) {
        let n = self.filas.len();
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
}

impl Panel {
    /// Opens the "Instalar driver" dialog for row `fila`, with the `.drv`
    /// files found and `fichero` (the one opened from the file browser, or
    /// the one chosen before) first.
    pub(super) fn abre_instala_con(&mut self, fila: usize, fichero: Option<std::path::PathBuf>) {
        let Some(f) = self.disp.filas.get(fila) else { return };
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let mut candidatos = dispositivos::candidatos(
            &dispositivos::dirs_candidatos(std::path::Path::new(&home)),
            (f.pci.vendor, f.pci.device),
        );
        if let Some(p) = fichero {
            candidatos.retain(|c| *c != p);
            candidatos.insert(0, p);
        }
        self.disp.foco = fila;
        self.disp.instala = Some(Instala {
            fila,
            candidatos,
            elegido: 0,
        });
        self.status.clear();
    }

    /// `lunarpanel --instalar FILE`: the devices page, with the dialog open
    /// for the device the file's name says (`mt7922-14c3-0616.drv`), else
    /// the first one without a driver.
    pub(super) fn instala_fichero(&mut self, path: std::path::PathBuf) {
        if let Some(g) = (0..self.cat.grupos.len()).find(|&g| {
            self.cat
                .del_grupo(g)
                .iter()
                .any(|&i| self.cat.claves[i].tipo == Tipo::Dispositivos)
        }) {
            self.grupo = g;
            self.busca.clear();
        }
        self.carga_disp();
        let filas = &self.disp.filas;
        let fila = dispositivos::ids_de_nombre(&path)
            .and_then(|(v, d)| filas.iter().position(|f| f.pci.vendor == v && f.pci.device == d))
            .or_else(|| filas.iter().position(|f| f.estado == Estado::SinDriver))
            .unwrap_or(0);
        self.abre_instala_con(fila, Some(path));
    }

    /// The `dispositivos` block of the page on screen, when it is that page.
    pub(super) fn pagina_disp(&self) -> Option<usize> {
        if !self.busca.trim().is_empty() {
            return None;
        }
        self.cat
            .del_grupo(self.grupo)
            .into_iter()
            .find(|&i| self.cat.claves[i].tipo == Tipo::Dispositivos)
    }

    /// Reads the machine's devices once the page is shown.
    pub(super) fn carga_disp(&mut self) {
        let Some(i) = self.pagina_disp() else { return };
        if self.disp.leido {
            return;
        }
        let nombres = Nombres::carga();
        self.disp.sin_nombres = nombres.vacio();
        self.disp.filas = dispositivos::lee(&self.cat.claves[i], &self.cat.drivers, &nombres);
        self.disp.foco = self.disp.foco.min(self.disp.filas.len().saturating_sub(1));
        self.disp.leido = true;
    }

    /// The count a page shows in the list: its devices, for this one.
    pub(super) fn cuenta_grupo(&self, g: usize) -> usize {
        let ids = self.cat.del_grupo(g);
        if self.disp.leido
            && ids
                .iter()
                .any(|&i| self.cat.claves[i].tipo == Tipo::Dispositivos)
        {
            return self.disp.filas.len();
        }
        ids.len()
    }
}

/// Where the page's parts are.
pub(super) struct DL {
    /// The x of each column: device, where, driver, state.
    pub cols: [i32; 4],
    pub head_y: i32,
    pub filas: Vec<Rect>,
    pub caben: usize,
    pub detalle: Rect,
    pub accion: Rect,
    pub copiar: Rect,
    pub informe: Rect,
    /// "Quitar driver", for a driver installed from the panel.
    pub quitar: Option<Rect>,
}

fn btn_w(label: &str) -> i32 {
    up4(Canvas::text_width(label) + 2 * PAD).max(BTN_W)
}

pub(super) fn layout_disp(l: &Layout, p: &Panel) -> DL {
    let rx = l.body.0 + PAD + 4;
    let rw = l.body.2 - 2 * PAD - 8;
    let head_y = l.body.1 + PAD / 2 + NAV_ROW_H;
    let first_y = head_y + HEAD_ROW_H;
    let detalle = (rx - 4, l.body.1 + l.body.3 - DETAIL_H - 8, rw + 8, DETAIL_H);
    let caben = ((detalle.1 - 8 - first_y) / DEV_ROW_H).max(1) as usize;
    let col = |pct: i32| rx + up4(rw * pct / 100);
    let filas = (0..p.disp.filas.len().saturating_sub(p.disp.top).min(caben))
        .map(|k| (rx, first_y + k as i32 * DEV_ROW_H, rw, DEV_ROW_H))
        .collect();
    // Right to left in the foot: the row's action, copy, report.
    let accion_l = accion_label(p).unwrap_or_else(|| p.lang.s(Key::panel_dev_enable_caps).into());
    let fy = l.apply.1;
    let wa = btn_w(&accion_l);
    let accion = (l.apply.0 + l.apply.2 - wa, fy, wa, BTN_H);
    let wc = btn_w(p.lang.s(Key::panel_dev_copy_caps));
    let copiar = (accion.0 - 12 - wc, fy, wc, BTN_H);
    let wi = btn_w(p.lang.s(Key::panel_dev_report_caps));
    let informe = (copiar.0 - 12 - wi, fy, wi, BTN_H);
    let quitar = instalado(p).then(|| {
        let wq = btn_w(p.lang.s(Key::panel_dev_remove_caps));
        (informe.0 - 12 - wq, fy, wq, BTN_H)
    });
    DL {
        cols: [rx, col(46), col(60), col(78)],
        head_y,
        filas,
        caben,
        detalle,
        accion,
        copiar,
        informe,
        quitar,
    }
}

/// Whether the focused row's driver is one installed from the panel.
fn instalado(p: &Panel) -> bool {
    p.disp
        .filas
        .get(p.disp.foco)
        .and_then(|f| f.driver)
        .and_then(|j| p.cat.drivers.get(j))
        .is_some_and(|d| d.instalado)
}

/// The focused row's action, as its button says it.
fn accion_label(p: &Panel) -> Option<String> {
    let f = p.disp.filas.get(p.disp.foco)?;
    let key = match f.accion() {
        Some(true) => Key::panel_dev_enable_caps,
        Some(false) => Key::panel_dev_return_caps,
        None if f.estado == Estado::SinDriver => Key::panel_dev_install_caps,
        None => return None,
    };
    Some(p.lang.s(key).into())
}

fn estado(e: Estado, lang: Lang) -> (&'static str, Rgb) {
    match e {
        Estado::Usuario => (lang.s(Key::panel_dev_user), ink().ok),
        Estado::Parado => (lang.s(Key::panel_dev_stopped), ink().warn),
        Estado::Nucleo => (lang.s(Key::panel_dev_kernel), ink().ok),
        Estado::Libre => (lang.s(Key::panel_dev_free), ink().cyan),
        Estado::SinDriver => (lang.s(Key::panel_dev_none), ink().amber),
        Estado::Sistema => (lang.s(Key::panel_dev_system), ink().dim),
    }
}

fn por_que(e: Estado, lang: Lang) -> &'static str {
    lang.s(match e {
        Estado::Usuario => Key::panel_dev_why_user,
        Estado::Parado => Key::panel_dev_why_stopped,
        Estado::Nucleo => Key::panel_dev_why_kernel,
        Estado::Libre => Key::panel_dev_why_free,
        Estado::SinDriver => Key::panel_dev_why_none,
        Estado::Sistema => Key::panel_dev_why_system,
    })
}

/// The page, under the head, search and page list `draw` has drawn.
pub(super) fn draw(cv: &mut Canvas, l: &Layout, p: &Panel, ci: usize) {
    let en = p.lang == Lang::En;
    let d = layout_disp(l, p);
    let (px, py, pw, ph) = l.panel;
    let rx = d.cols[0];
    let rw = l.body.2 - 2 * PAD - 8;
    let drivers = &p.cat.drivers;

    // Title, as on every page, with the device count.
    let sec_y = l.body.1 + PAD / 2 + (NAV_ROW_H - GLYPH_H) / 2;
    let title = p.cat.claves[ci].nombre(en).to_uppercase();
    let stw = cv.text_bold(&title, rx, sec_y, ink().text);
    let n = format!("{:02}", p.disp.filas.len());
    let nw = Canvas::text_width(&n);
    cv.hline(rx + stw + 12, sec_y + GLYPH_H / 2, rw - stw - nw - 24, ink().hairline, 1.0);
    cv.text(&n, rx + rw - nw, sec_y, ink().muted);

    let heads = [
        p.lang.s(Key::mon_device),
        p.lang.s(Key::panel_dev_col_where),
        p.lang.s(Key::panel_dev_col_driver),
        p.lang.s(Key::mon_state),
    ];
    for (x, t) in d.cols.iter().zip(heads) {
        cv.text(t, *x, d.head_y + (HEAD_ROW_H - GLYPH_H) / 2 - 2, ink().dim);
    }
    cv.hline(rx, d.head_y + HEAD_ROW_H - 2, rw, ink().hairline, 1.0);
    if p.disp.filas.is_empty() {
        cv.text(p.lang.s(Key::panel_dev_no_bus), rx, d.head_y + HEAD_ROW_H + 12, ink().muted);
    }

    for (k, r) in d.filas.iter().enumerate() {
        let i = p.disp.top + k;
        let f = &p.disp.filas[i];
        let focused = i == p.disp.foco;
        let (x, y, fw, fh) = *r;
        if focused {
            glass::cell(cv, x - 6, y + 1, fw + 12, fh - 2, ink().cyan, 0.08);
            glass::brackets(cv, x - 8, y - 1, fw + 16, fh + 2, 8, 2, ink().gold);
        }
        let name_w = d.cols[1] - x - PAD;
        cv.text(
            &fit(&f.nombre, name_w),
            x,
            y + 4,
            if focused { ink().text } else { ink().muted },
        );
        let sub = match &f.fabricante {
            Some(m) => format!("{} · {}", f.pci.ids(), m),
            None => f.pci.ids(),
        };
        cv.text(&fit(&sub, name_w), x, y + 4 + GLYPH_H + 2, ink().dim);
        cv.text(f.pci.donde(), d.cols[1], y + (fh - GLYPH_H) / 2, ink().muted);
        let drv_w = d.cols[3] - d.cols[2] - PAD;
        let drv = f.driver.and_then(|j| drivers.get(j)).map(|x| x.es.as_str()).unwrap_or("—");
        cv.text(&fit(drv, drv_w), d.cols[2], y + 4, ink().text);
        if let Some(it) = &f.item {
            let col = if f.pedido { ink().gold } else { ink().dim };
            cv.text(&fit(it, drv_w), d.cols[2], y + 4 + GLYPH_H + 2, col);
        }
        let (word, col) = estado(f.estado, p.lang);
        let dy = y + (fh - DOT) / 2;
        cv.fill_rect_a(d.cols[3], dy, DOT, DOT, col, 1.0);
        cv.text(
            &fit(word, x + fw - d.cols[3] - DOT - 8),
            d.cols[3] + DOT + 8,
            y + (fh - GLYPH_H) / 2,
            col,
        );
        cv.hline(x, y + fh - 1, fw, ink().hairline, 0.5);
    }
    let shown = d.filas.len();
    if p.disp.top > 0 || p.disp.top + shown < p.disp.filas.len() {
        let t = format!("{}–{} / {}", p.disp.top + 1, p.disp.top + shown, p.disp.filas.len());
        let tw = Canvas::text_width(&t);
        cv.text(&t, rx + rw - tw, d.detalle.1 - GLYPH_H - 6, ink().dim);
    }

    // The focused device, said in full.
    let dr = d.detalle;
    glass::cell(cv, dr.0, dr.1, dr.2, dr.3, ink().well, 1.0);
    frame(cv, dr, ink().hairline);
    if let Some(f) = p.disp.filas.get(p.disp.foco) {
        // The whole name, which the row may have cut.
        let head = match &f.fabricante {
            Some(m) => format!("{m} · {}", f.nombre),
            None => f.nombre.clone(),
        };
        cv.text_bold(&fit(&head, dr.2 - 2 * PAD), dr.0 + PAD, dr.1 + 10, ink().gold);
        let mut why = f.pci.ids();
        if let Some(c) = f.clase.as_ref().filter(|c| **c != f.nombre) {
            why += &format!(" · {c}");
        }
        why += &format!(" · {}", por_que(f.estado, p.lang));
        if let Some(it) = &f.item {
            why += &format!(" · {it}");
        }
        if f.pedido {
            why += &format!(" · {}", p.lang.s(Key::panel_dev_named));
        }
        cv.text(&fit(&why, dr.2 - 2 * PAD), dr.0 + PAD, dr.1 + 10 + GLYPH_H + 8, ink().muted);
    }

    // Foot.
    let top = py + ph - FOOT_H;
    cv.hline(px + PAD, top, pw - 2 * PAD, ink().hairline, 1.0);
    let accion = accion_label(p);
    button(
        cv,
        d.accion,
        accion.as_deref().unwrap_or(p.lang.s(Key::panel_dev_enable_caps)),
        accion.is_some(),
    );
    button(cv, d.copiar, p.lang.s(Key::panel_dev_copy_caps), false);
    button(cv, d.informe, p.lang.s(Key::panel_dev_report_caps), false);
    if let Some(q) = d.quitar {
        button(cv, q, p.lang.s(Key::panel_dev_remove_caps), false);
    }
    if p.disp.instala.is_some() {
        draw_instala(cv, l, p);
    }
    let fy = top + (FOOT_H - GLYPH_H) / 2;
    let room = d.quitar.unwrap_or(d.informe).0 - px - 2 * PAD;
    if p.disp.instala.is_some() {
        return;
    }
    if !p.status.is_empty() {
        let col = match p.tono {
            Tono::Plano => ink().muted,
            Tono::Bien => ink().cyan,
            Tono::Mal => ink().warn,
        };
        cv.text(&fit(&p.status, room), px + PAD, fy, col);
        return;
    }
    let total = p.disp.filas.len();
    let sin = p.disp.filas.iter().filter(|f| f.estado == Estado::SinDriver).count();
    let con = p
        .disp
        .filas
        .iter()
        .filter(|f| !matches!(f.estado, Estado::SinDriver | Estado::Sistema))
        .count();
    let mut x = px + PAD;
    for (num, word, col) in [
        (total, p.lang.s(Key::panel_dev_devices), ink().cyan),
        (con, p.lang.s(Key::panel_dev_with_driver), ink().cyan),
        (sin, p.lang.s(Key::panel_dev_none), ink().amber),
    ] {
        x += cv.text(&num.to_string(), x, fy, col);
        x += cv.text(&format!(" {word}   "), x, fy, ink().muted);
    }
    if p.disp.sin_nombres && x < px + PAD + room {
        cv.text(&fit(p.lang.s(Key::panel_dev_no_names), px + PAD + room - x), x, fy, ink().warn);
    }
}

// ── the "Instalar driver" dialog ─────────────────────────────────────────────

const INST_W: i32 = 760;
const INST_H: i32 = 480;
/// Where the fields start, from the dialog's left edge.
const INST_FIELD_X: i32 = 160;
const INST_FIELD_H: i32 = 28;

/// Where the dialog's parts are.
pub(super) struct IL {
    pub caja: Rect,
    pub campo: Rect,
    pub cancelar: Rect,
    pub ok: Rect,
}

pub(super) fn layout_instala(l: &Layout, p: &Panel) -> IL {
    let (px, py, pw, ph) = l.panel;
    let w = INST_W.min(pw - 2 * PAD);
    let caja = (px + up4((pw - w) / 2), py + up4((ph - INST_H) / 2), w, INST_H);
    let campo = (
        caja.0 + INST_FIELD_X,
        caja.1 + 84,
        w - INST_FIELD_X - 3 * PAD / 2,
        INST_FIELD_H,
    );
    let by = caja.1 + INST_H - BTN_H - 20;
    let wo = btn_w(p.lang.s(Key::panel_inst_ok_caps));
    let ok = (caja.0 + w - 3 * PAD / 2 - wo, by, wo, BTN_H);
    let wc = btn_w(p.lang.s(Key::panel_inst_cancel_caps));
    let cancelar = (ok.0 - 12 - wc, by, wc, BTN_H);
    IL {
        caja,
        campo,
        cancelar,
        ok,
    }
}

fn draw_instala(cv: &mut Canvas, l: &Layout, p: &Panel) {
    let Some(ins) = &p.disp.instala else { return };
    let Some(f) = p.disp.filas.get(ins.fila) else { return };
    let lang = p.lang;
    let il = layout_instala(l, p);
    let (px, py, pw, ph) = l.panel;
    cv.fill_rect_a(px, py, pw, ph, ink().ground, 0.55);
    let (x, y, w, h) = il.caja;
    glass::panel(cv, x, y, w, h, ink().surface);
    frame(cv, il.caja, ink().hairline);
    glass::brackets(cv, x - 3, y - 3, w + 6, h + 6, 12, 2, ink().gold);
    let lx = x + 3 * PAD / 2;
    let room = w - 3 * PAD;
    cv.text_bold(lang.s(Key::panel_inst_title), lx, y + 20, ink().gold);
    let mut who = f.nombre.clone();
    if let Some(m) = &f.fabricante {
        who = format!("{m} · {who}");
    }
    who += &format!(" · {} · {}", f.pci.ids(), f.pci.donde());
    cv.text(&fit(&who, room), lx, y + 44, ink().muted);
    cv.hline(lx, y + 68, room, ink().hairline, 1.0);

    // Driver: the file chosen among the ones found.
    let c = il.campo;
    let fty = c.1 + (INST_FIELD_H - GLYPH_H) / 2;
    cv.text(lang.s(Key::panel_inst_driver), lx, fty, ink().text);
    glass::cell(cv, c.0, c.1, c.2, c.3, ink().well, 1.0);
    frame(cv, c, ink().cyan);
    let fw = c.2 - 2 * 10;
    match ins.candidatos.get(ins.elegido) {
        Some(path) => {
            let mut t = corto(path);
            if ins.candidatos.len() > 1 {
                t = format!("{t}   {}/{}", ins.elegido + 1, ins.candidatos.len());
            }
            cv.text(&fit(&t, fw), c.0 + 10, fty, ink().text);
            cv.text(&fit(lang.s(Key::panel_inst_driver_hint), c.2), c.0, c.1 + c.3 + 6, ink().dim);
        }
        None => {
            cv.text("—", c.0 + 10, fty, ink().dim);
            cv.text(&fit(lang.s(Key::panel_inst_no_file), c.2), c.0, c.1 + c.3 + 6, ink().warn);
        }
    }

    // Service: computed from the ids.
    let name = dispositivos::servicio(&f.pci);
    let sy = c.1 + 70;
    let s = (c.0, sy, c.2, INST_FIELD_H);
    let sty = sy + (INST_FIELD_H - GLYPH_H) / 2;
    cv.text(lang.s(Key::panel_inst_service), lx, sty, ink().text);
    glass::cell(cv, s.0, s.1, s.2, s.3, ink().well, 1.0);
    frame(cv, s, ink().hairline);
    cv.text(&name, s.0 + 10, sty, ink().text);
    cv.text(&fit(lang.s(Key::panel_inst_service_hint), s.2), s.0, sy + s.3 + 6, ink().dim);

    // What it will do.
    let item = dispositivos::item_pci(&f.pci);
    let wy = sy + 76;
    cv.text(lang.s(Key::panel_inst_will), lx, wy, ink().dim);
    let steps = [
        (
            Some(1),
            lang.s(Key::panel_inst_step_copy)
                .replace("{}", &format!("/usr/lib/eclipse/udrivers/{name}")),
        ),
        (
            Some(2),
            lang.s(Key::panel_inst_step_service)
                .replace("{}", &format!("/etc/eclipse/services/{name}.service")),
        ),
        (None, format!("respawn · udriver = {item}")),
        (Some(3), lang.s(Key::panel_inst_step_boot).replace("{}", &item)),
        (Some(4), lang.s(Key::panel_inst_step_reboot).to_string()),
    ];
    for (k, (n, t)) in steps.iter().enumerate() {
        let ty = wy + 22 + k as i32 * 22;
        match n {
            Some(n) => {
                cv.text_bold(&n.to_string(), lx, ty, ink().cyan);
                cv.text(&fit(t, room - 24), lx + 24, ty, ink().text);
            }
            None => {
                cv.text(&fit(t, room - 48), lx + 48, ty, ink().muted);
            }
        }
    }

    // The warning.
    let ay = wy + 22 + steps.len() as i32 * 22 + 8;
    let a = (lx, ay, room, 2 * GLYPH_H + 20);
    glass::cell(cv, a.0, a.1, a.2, a.3, ink().warn, 0.08);
    frame(cv, a, ink().warn);
    cv.text(&fit(lang.s(Key::panel_inst_warn1), room - 24), lx + 12, ay + 8, ink().warn);
    cv.text(
        &fit(lang.s(Key::panel_inst_warn2), room - 24),
        lx + 12,
        ay + 8 + GLYPH_H + 4,
        ink().warn,
    );

    // Foot of the dialog.
    let by = il.ok.1 + (BTN_H - GLYPH_H) / 2;
    if !p.status.is_empty() {
        cv.text(&fit(&p.status, il.cancelar.0 - lx - PAD), lx, by, ink().warn);
    } else {
        cv.text(
            &fit(lang.s(Key::panel_inst_keys), il.cancelar.0 - lx - PAD),
            lx,
            by,
            ink().dim,
        );
    }
    button(cv, il.cancelar, lang.s(Key::panel_inst_cancel_caps), false);
    button(
        cv,
        il.ok,
        lang.s(Key::panel_inst_ok_caps),
        !ins.candidatos.is_empty(),
    );
}

// ── what the page does ───────────────────────────────────────────────────────

impl State {
    /// Hands the focused device over (or back) at the next boot.
    pub(super) fn disp_accion(&mut self) {
        let Some(ci) = self.panel.pagina_disp() else { return };
        let Some(f) = self.panel.disp.filas.get(self.panel.disp.foco) else {
            return;
        };
        if f.accion().is_none() && f.estado == Estado::SinDriver {
            self.abre_instala();
            return;
        }
        let (Some(on), Some(item)) = (f.accion(), f.item.clone()) else {
            return;
        };
        match dispositivos::cambia(&self.panel.cat.claves[ci], &item, on) {
            Ok(()) => {
                let lang = self.panel.lang;
                let what = lang.s(if on {
                    Key::panel_dev_enable_caps
                } else {
                    Key::panel_dev_return_caps
                });
                self.panel.status = format!("{item} {what} · {}", lang.s(Key::panel_on_reboot));
                self.panel.tono = Tono::Bien;
                self.panel.disp.leido = false;
                self.panel.carga_disp();
            }
            Err(e) => {
                self.panel.status = e;
                self.panel.tono = Tono::Mal;
            }
        }
    }

    /// Opens the dialog for the focused device, with the files found.
    pub(super) fn abre_instala(&mut self) {
        let fila = self.panel.disp.foco;
        self.panel.abre_instala_con(fila, None);
    }

    /// The chosen file becomes the device's driver, or the installed one
    /// goes (`fichero` None); either takes effect at the next boot.
    fn instala(&mut self, fila: usize, fichero: Option<std::path::PathBuf>) {
        let Some(ci) = self.panel.pagina_disp() else { return };
        let Some(f) = self.panel.disp.filas.get(fila) else { return };
        let pci = f.pci.clone();
        let lang = self.panel.lang;
        match dispositivos::instala(&self.panel.cat.claves[ci], &pci, fichero.as_deref()) {
            Ok(()) => {
                self.panel.disp.instala = None;
                let what = lang.s(if fichero.is_some() {
                    Key::panel_inst_done
                } else {
                    Key::panel_inst_removed
                });
                self.panel.status = format!(
                    "{} {what} · {}",
                    dispositivos::servicio(&pci),
                    lang.s(Key::panel_on_reboot)
                );
                self.panel.tono = Tono::Bien;
                // The installed block is read again: it names the device.
                let dirs: Vec<&std::path::Path> =
                    claves::DIRS.iter().map(std::path::Path::new).collect();
                self.panel.cat.drivers = Catalogo::carga(&dirs).drivers;
                self.panel.disp.leido = false;
                self.panel.carga_disp();
            }
            Err(e) => {
                self.panel.status = e;
                self.panel.tono = Tono::Mal;
            }
        }
    }

    /// INSTALAR in the dialog.
    fn confirma_instala(&mut self) {
        let Some(ins) = &self.panel.disp.instala else { return };
        let Some(path) = ins.candidatos.get(ins.elegido).cloned() else {
            return;
        };
        let fila = ins.fila;
        self.instala(fila, Some(path));
    }

    /// QUITAR DRIVER on the focused row.
    pub(super) fn disp_quita(&mut self) {
        if instalado(&self.panel) {
            self.instala(self.panel.disp.foco, None);
        }
    }

    /// A key while the dialog is open: it takes them all.
    fn instala_key(&mut self, key: u32) {
        let Some(ins) = self.panel.disp.instala.as_mut() else { return };
        let n = ins.candidatos.len();
        match key {
            KEY_ESC_WL => {
                self.panel.disp.instala = None;
                self.panel.status.clear();
            }
            KEY_LEFT_WL if n > 0 => ins.elegido = (ins.elegido + n - 1) % n,
            KEY_RIGHT_WL | KEY_TAB_WL if n > 0 => ins.elegido = (ins.elegido + 1) % n,
            // Another device: the files are sorted again for it.
            KEY_UP_WL | KEY_DOWN_WL => {
                let rows = self.panel.disp.filas.len();
                let fila = ins.fila;
                let elegido = ins.candidatos.get(ins.elegido).cloned();
                if rows > 0 {
                    let fila = if key == KEY_UP_WL {
                        (fila + rows - 1) % rows
                    } else {
                        (fila + 1) % rows
                    };
                    self.panel.abre_instala_con(fila, elegido);
                }
            }
            KEY_ENTER_WL | KEY_KPENTER_WL => self.confirma_instala(),
            _ => {}
        }
    }

    /// A click while the dialog is open.
    fn instala_click(&mut self, x: i32, y: i32) {
        let l = self.layout();
        let il = layout_instala(&l, &self.panel);
        if hit(il.ok, x, y) {
            self.confirma_instala();
        } else if hit(il.cancelar, x, y) {
            self.panel.disp.instala = None;
            self.panel.status.clear();
        } else if hit(il.campo, x, y) {
            if let Some(ins) = self.panel.disp.instala.as_mut() {
                let n = ins.candidatos.len().max(1);
                ins.elegido = (ins.elegido + 1) % n;
            }
        }
    }

    /// The focused device's ids and names, to the clipboard.
    pub(super) fn disp_copia(&mut self) {
        let Some(f) = self.panel.disp.filas.get(self.panel.disp.foco) else {
            return;
        };
        let line = f.linea();
        let ok = std::process::Command::new("lunarclip")
            .arg("--copy")
            .arg(&line)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok();
        self.panel.status = if ok {
            format!("{} · {line}", self.panel.lang.s(Key::file_copied))
        } else {
            line
        };
        self.panel.tono = if ok { Tono::Bien } else { Tono::Plano };
    }

    /// Every device and its driver, to a file in the home directory.
    pub(super) fn disp_informe(&mut self) {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let path = std::path::Path::new(&home).join("dispositivos.txt");
        let text = dispositivos::informe(&self.panel.disp.filas, &self.panel.cat.drivers);
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.panel.status = format!(
                    "{} {}",
                    self.panel.lang.s(Key::hunt_report_saved_to),
                    path.display()
                );
                self.panel.tono = Tono::Bien;
            }
            Err(e) => {
                self.panel.status = format!("{}: {e}", path.display());
                self.panel.tono = Tono::Mal;
            }
        }
    }

    /// A key on this page; `false` when it is not one the page takes.
    pub(super) fn disp_key(&mut self, key: u32) -> bool {
        if self.panel.disp.instala.is_some() {
            self.instala_key(key);
            return true;
        }
        let l = self.layout();
        let caben = layout_disp(&l, &self.panel).caben;
        match key {
            KEY_UP_WL => self.panel.disp.mueve(-1, caben),
            KEY_DOWN_WL => self.panel.disp.mueve(1, caben),
            KEY_PGUP_WL => self.panel.disp.mueve(-(caben as i32), caben),
            KEY_PGDN_WL => self.panel.disp.mueve(caben as i32, caben),
            KEY_ENTER_WL | KEY_KPENTER_WL => self.disp_accion(),
            KEY_SPACE => self.disp_accion(),
            KEY_LEFT_WL | KEY_RIGHT_WL => {}
            _ => return false,
        }
        if !matches!(key, KEY_ENTER_WL | KEY_KPENTER_WL | KEY_SPACE) {
            self.panel.status.clear();
        }
        true
    }

    /// A click on this page; `false` when it is not on one of its parts.
    pub(super) fn disp_click(&mut self, x: i32, y: i32) -> bool {
        if self.panel.disp.instala.is_some() {
            self.instala_click(x, y);
            return true;
        }
        let l = self.layout();
        let d = layout_disp(&l, &self.panel);
        if d.quitar.is_some_and(|q| hit(q, x, y)) {
            self.disp_quita();
        } else if hit(d.accion, x, y) {
            self.disp_accion();
        } else if hit(d.copiar, x, y) {
            self.disp_copia();
        } else if hit(d.informe, x, y) {
            self.disp_informe();
        } else if let Some(k) = d.filas.iter().position(|r| hit(*r, x, y)) {
            self.panel.disp.foco = self.panel.disp.top + k;
            self.panel.status.clear();
        } else {
            return false;
        }
        true
    }
}

/// A made-up machine for `--dump`: two NVIDIA cards, an NVMe disk, a SATA
/// controller, a wireless card nobody drives. Names come from this host's
/// `pci.ids` when it has one.
pub(super) fn muestra(p: &mut Panel) {
    use lunarbar::dispositivos::{calcula, Pci};
    use lunarbar::sysmon::Svc;
    let pci = |bdf: &str, vendor: u16, device: u16, class: u32| Pci {
        bdf: bdf.into(),
        vendor,
        device,
        class,
    };
    let bus = [
        pci("0000:00:00.0", 0x8086, 0x4660, 0x060000),
        pci("0000:00:14.0", 0x8086, 0x7ae0, 0x0c0330),
        pci("0000:00:17.0", 0x8086, 0x7ae2, 0x010601),
        pci("0000:00:1f.3", 0x8086, 0x7ad0, 0x040300),
        pci("0000:00:1f.4", 0x8086, 0x7aa3, 0x0c0500),
        pci("0000:01:00.0", 0x10de, 0x2684, 0x030000),
        pci("0000:01:00.1", 0x10de, 0x22ba, 0x040300),
        pci("0000:02:00.0", 0x10de, 0x2204, 0x030000),
        pci("0000:03:00.0", 0x144d, 0xa80a, 0x010802),
        pci("0000:04:00.0", 0x8086, 0x15f3, 0x020000),
        pci("0000:05:00.0", 0x14c3, 0x0616, 0x028000),
    ];
    let svc = |name: &str, dev: &str| Svc {
        name: name.into(),
        state: "running".into(),
        udriver: Some(dev.into()),
        ..Default::default()
    };
    let svcs = [svc("unvme", "nvme@0"), svc("uahci", "ahci@0"), svc("uhda", "hda@0")];
    let nombres = Nombres::carga();
    p.disp.sin_nombres = nombres.vacio();
    p.disp.filas = calcula(
        &bus,
        &p.cat.drivers,
        &nombres,
        "nvme@0\nahci@0\nhda@0\n",
        &svcs,
        "LOG=error:udriver=xhci@0",
    );
    p.disp.foco = 10;
    p.disp.top = 2;
    p.disp.leido = true;
}
