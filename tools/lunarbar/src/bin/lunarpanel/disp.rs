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
    DL {
        cols: [rx, col(46), col(60), col(78)],
        head_y,
        filas,
        caben,
        detalle,
        accion,
        copiar,
        informe,
    }
}

/// The focused row's action, as its button says it.
fn accion_label(p: &Panel) -> Option<String> {
    let f = p.disp.filas.get(p.disp.foco)?;
    Some(
        p.lang
            .s(match f.accion()? {
                true => Key::panel_dev_enable_caps,
                false => Key::panel_dev_return_caps,
            })
            .into(),
    )
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
    let fy = top + (FOOT_H - GLYPH_H) / 2;
    let room = d.informe.0 - px - 2 * PAD;
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

// ── what the page does ───────────────────────────────────────────────────────

impl State {
    /// Hands the focused device over (or back) at the next boot.
    pub(super) fn disp_accion(&mut self) {
        let Some(ci) = self.panel.pagina_disp() else { return };
        let Some(f) = self.panel.disp.filas.get(self.panel.disp.foco) else {
            return;
        };
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
        let l = self.layout();
        let d = layout_disp(&l, &self.panel);
        if hit(d.accion, x, y) {
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
