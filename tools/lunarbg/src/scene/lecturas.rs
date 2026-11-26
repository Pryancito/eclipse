//! The tecno face's readings ("ECLIPSE · LECTURAS"): a column of live system
//! measures beside the clock, placed by the face's own geometry.
//!
//! Nothing here is positioned by hand. With `n` rows a [`PASO`] apart and
//! centred on the face, row `i` sits at `dy = (i - (n-1)/2) · PASO`. Its tick
//! is the point of the outer ring at that height, at the angle
//! `θ = asin(dy / R)`, so the ticks follow the ring; a hairline runs level
//! from that point (`x = cx + √(R² − dy²)`) to the column, which stands
//! [`COL_DX`] outside the ring. More or fewer rows, or another side, and the
//! same sums put everything somewhere else. The clock rings (hour, minute,
//! seconds) are never touched: the readings only use the outer ring, which
//! tells no time.
//!
//! What is shown is data too: every row is a clave in `/etc/eclipse/entorno`
//! (`LUNARBG_LECTURA_*`, `si`/`no`), the column's side is `LUNARBG_LECTURAS`
//! (`derecha`, `izquierda` or `no`), and the control panel writes them
//! (`tools/lunarbar/claves.d/62-fondo.claves`). A row whose source this
//! machine does not have (no thermal zone, say) is left out, and the column
//! is recomputed for the rows that are left.
//!
//! The measures come from `/proc` and `/sys`, read at most once a second
//! however fast the face animates.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use eclipse_i18n::Lang;

use super::{PixBuf, Rgb};

// ------------------------------------------------------------- geometry

/// Row pitch, design units: ⌈2.5 · em⌉ on the 4 px grid, em = [`EM`].
pub(super) const PASO: f32 = 32.0;
/// Text size of the column, design units.
pub(super) const EM: f32 = 12.0;
/// From the outer ring to the column's dot.
pub(super) const COL_DX: f32 = 42.0;
/// Width of the column, from its dot to the right edge of the values.
pub(super) const COL_W: f32 = 316.0;
/// Label, meter and graph offsets inside the column.
const LABEL_DX: f32 = 14.0;
const BAR_DX: f32 = 128.0;
/// The header sits one pitch above the first row.
const HEAD_DY: f32 = PASO + 4.0;
/// Segments of a meter and samples in the network trace.
const SEGS: usize = 10;
const TRAZA: usize = 30;

const TEXT: Rgb = rgb(0xe6, 0xf1, 0xff);
const HAIR: Rgb = rgb(0x1d, 0x35, 0x50);
const WARN: Rgb = rgb(0xff, 0x8a, 0x5c);

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

// ------------------------------------------------------------- settings

/// Which side of the face the column stands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lado {
    Derecha,
    Izquierda,
}

/// One reading the column can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Id {
    Carga,
    Cpu,
    Memoria,
    Red,
    Disco,
    Temperatura,
    EnMarcha,
    Procesos,
}

/// Every reading, in the column's order, with the clave that turns it off.
pub const LECTURAS: [(Id, &str); 8] = [
    (Id::Carga, "LUNARBG_LECTURA_CARGA"),
    (Id::Cpu, "LUNARBG_LECTURA_CPU"),
    (Id::Memoria, "LUNARBG_LECTURA_MEMORIA"),
    (Id::Red, "LUNARBG_LECTURA_RED"),
    (Id::Disco, "LUNARBG_LECTURA_DISCO"),
    (Id::Temperatura, "LUNARBG_LECTURA_TEMPERATURA"),
    (Id::EnMarcha, "LUNARBG_LECTURA_EN_MARCHA"),
    (Id::Procesos, "LUNARBG_LECTURA_PROCESOS"),
];

/// What the face shows, from its claves.
#[derive(Clone, Debug, PartialEq)]
pub struct Ajustes {
    /// The column's side, or `None` for no column.
    pub lado: Option<Lado>,
    /// The readings asked for, in order.
    pub filas: Vec<Id>,
    /// The clock on the rings (hour, minute, seconds).
    pub reloj: bool,
    /// The date and Moon line under the caption.
    pub fecha: bool,
}

/// The face's settings from `get` (a clave's value by name). Anything that is
/// not a known word keeps the default, which is everything on, column right.
pub fn ajustes_de(get: impl Fn(&str) -> Option<String>) -> Ajustes {
    let no = |name: &str| get(name).is_some_and(|v| v.trim().eq_ignore_ascii_case("no"));
    let lado = match get("LUNARBG_LECTURAS").as_deref().map(str::trim) {
        Some("no") => None,
        Some("izquierda") => Some(Lado::Izquierda),
        _ => Some(Lado::Derecha),
    };
    Ajustes {
        lado,
        filas: LECTURAS
            .iter()
            .filter(|(_, k)| !no(k))
            .map(|(id, _)| *id)
            .collect(),
        reloj: !no("LUNARBG_RELOJ"),
        fecha: !no("LUNARBG_FECHA"),
    }
}

/// The settings in force, read ONCE per process like the language:
/// `eclipse-entorno` restarts the wallpaper when one changes.
pub fn ajustes() -> &'static Ajustes {
    static A: OnceLock<Ajustes> = OnceLock::new();
    A.get_or_init(|| ajustes_de(lunartext::entorno::var))
}

/// The column's horizontal extent `(x0, x1)` in buffer pixels for a face at
/// `cx` with scale `s` and squeeze `sx`: what the layout must redraw.
pub fn extension(lado: Lado, cx: f32, s: f32, sx: f32) -> (f32, f32) {
    let near = (super::tecno::R_OUTER * sx + COL_DX) * s;
    match lado {
        Lado::Derecha => (cx + near - 4.0 * s, cx + near + COL_W * s + 2.0 * s),
        Lado::Izquierda => (cx - near - COL_W * s - 2.0 * s, cx - near + 4.0 * s),
    }
}

/// Row `i` of `n`: its height from the centre, design units.
pub fn dy(i: usize, n: usize) -> f32 {
    (i as f32 - (n as f32 - 1.0) / 2.0) * PASO
}

/// Where the level line of a row at `dy` leaves a ring of radius `r`:
/// `√(r² − dy²)` from the centre, or `None` if the row misses the ring.
pub fn sobre_anillo(r: f32, dy: f32) -> Option<f32> {
    let k = r * r - dy * dy;
    (k >= 0.0).then(|| k.sqrt())
}

// ------------------------------------------------------------- measures

/// One reading, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Fila {
    pub etiqueta: String,
    pub valor: String,
    /// A meter, 0..1.
    pub barra: Option<f32>,
    /// A trace, each sample 0..1.
    pub traza: Option<Vec<f32>>,
    /// Draw the value in the warning colour.
    pub aviso: bool,
}

/// What was read from the system at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Muestra {
    pub carga: Option<[f32; 3]>,
    /// Busy fraction and thread count.
    pub cpu: Option<(f32, usize)>,
    /// Used and total, KiB.
    pub mem: Option<(u64, u64)>,
    /// Received and sent, bytes per second.
    pub red: Option<(f64, f64)>,
    pub red_traza: Vec<f32>,
    /// Used and total bytes of `/`.
    pub disco: Option<(u64, u64)>,
    /// The hottest thermal zone, °C.
    pub temp: Option<i32>,
    pub en_marcha: Option<u64>,
    pub procesos: Option<usize>,
}

/// `/proc/loadavg`'s three averages.
pub fn carga_de(text: &str) -> Option<[f32; 3]> {
    let mut it = text.split_whitespace().map(|v| v.parse::<f32>().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

/// `(busy, total)` jiffies of `/proc/stat`'s aggregate line, and how many
/// `cpuN` lines (threads) follow it.
pub fn cpu_de(text: &str) -> Option<(u64, u64, usize)> {
    let mut lines = text.lines();
    let first = lines.next()?;
    let mut f = first.split_whitespace();
    if f.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = f.filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    // idle and iowait are the 4th and 5th columns.
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    let hilos = text
        .lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .count();
    Some((total.saturating_sub(idle), total, hilos.max(1)))
}

/// `(used, total)` KiB from `/proc/meminfo` (used = total − available).
pub fn mem_de(text: &str) -> Option<(u64, u64)> {
    let get = |k: &str| {
        text.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l[k.len()..].split_whitespace().next()?.parse::<u64>().ok())
    };
    let total = get("MemTotal:")?;
    let avail = get("MemAvailable:").or_else(|| get("MemFree:"))?;
    (total > 0).then(|| (total.saturating_sub(avail), total))
}

/// Received and sent bytes over every interface but loopback in
/// `/proc/net/dev`.
pub fn red_de(text: &str) -> Option<(u64, u64)> {
    let mut any = false;
    let (mut rx, mut tx) = (0u64, 0u64);
    for l in text.lines() {
        let Some((name, rest)) = l.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let v: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() >= 9 {
            rx += v[0];
            tx += v[8];
            any = true;
        }
    }
    any.then_some((rx, tx))
}

/// Seconds since boot from `/proc/uptime`.
pub fn en_marcha_de(text: &str) -> Option<u64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|s| s as u64)
}

fn leer(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn disco() -> Option<(u64, u64)> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a zeroed struct statvfs fills in.
    if unsafe { libc::statvfs(c"/".as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bs = st.f_frsize as u64;
    let total = st.f_blocks as u64 * bs;
    let libre = st.f_bavail as u64 * bs;
    (total > 0).then(|| (total.saturating_sub(libre), total))
}

fn temperatura() -> Option<i32> {
    let dir = std::fs::read_dir("/sys/class/thermal").ok()?;
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("thermal_zone"))
        .filter_map(|e| {
            leer(&format!("{}/temp", e.path().display()))?
                .trim()
                .parse::<i64>()
                .ok()
        })
        // millidegrees; a zone that reads 0 or nonsense is not a sensor.
        .map(|m| (m / 1000) as i32)
        .filter(|c| (1..150).contains(c))
        .max()
}

fn procesos() -> Option<usize> {
    let n = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .count();
    (n > 0).then_some(n)
}

struct Lector {
    /// Second of the last read (the face's clock).
    t: i64,
    cpu: Option<(u64, u64)>,
    red: Option<(u64, u64, std::time::Instant)>,
    traza: VecDeque<f64>,
    m: Muestra,
}

/// The latest measures, read again when the second `t` is a new one.
pub fn muestra(t: i64) -> Muestra {
    static L: OnceLock<Mutex<Lector>> = OnceLock::new();
    let l = L.get_or_init(|| {
        Mutex::new(Lector {
            t: i64::MIN,
            cpu: None,
            red: None,
            traza: VecDeque::new(),
            m: Muestra::default(),
        })
    });
    let mut l = l.lock().unwrap_or_else(|p| p.into_inner());
    if l.t != t {
        l.t = t;
        leer_todo(&mut l);
    }
    l.m.clone()
}

fn leer_todo(l: &mut Lector) {
    let quiere = |id| ajustes().filas.contains(&id);
    let mut m = Muestra::default();
    if quiere(Id::Carga) {
        m.carga = leer("/proc/loadavg").as_deref().and_then(carga_de);
    }
    if quiere(Id::Cpu) {
        if let Some((busy, total, hilos)) = leer("/proc/stat").as_deref().and_then(cpu_de) {
            // Since the last read; since boot on the first one.
            let (b0, t0) = l.cpu.unwrap_or((0, 0));
            let (db, dt) = (busy.saturating_sub(b0), total.saturating_sub(t0));
            let frac = if dt > 0 { db as f32 / dt as f32 } else { 0.0 };
            m.cpu = Some((frac.clamp(0.0, 1.0), hilos));
            l.cpu = Some((busy, total));
        }
    }
    if quiere(Id::Memoria) {
        m.mem = leer("/proc/meminfo").as_deref().and_then(mem_de);
    }
    if quiere(Id::Red) {
        if let Some((rx, tx)) = leer("/proc/net/dev").as_deref().and_then(red_de) {
            let now = std::time::Instant::now();
            if let Some((rx0, tx0, t0)) = l.red {
                let secs = now.duration_since(t0).as_secs_f64().max(0.05);
                let r = (
                    rx.saturating_sub(rx0) as f64 / secs,
                    tx.saturating_sub(tx0) as f64 / secs,
                );
                m.red = Some(r);
                l.traza.push_back(r.0 + r.1);
                while l.traza.len() > TRAZA {
                    l.traza.pop_front();
                }
            } else {
                m.red = Some((0.0, 0.0));
            }
            l.red = Some((rx, tx, now));
            let top = l.traza.iter().copied().fold(1.0, f64::max);
            m.red_traza = l.traza.iter().map(|v| (v / top) as f32).collect();
        }
    }
    if quiere(Id::Disco) {
        m.disco = disco();
    }
    if quiere(Id::Temperatura) {
        m.temp = temperatura();
    }
    if quiere(Id::EnMarcha) {
        m.en_marcha = leer("/proc/uptime").as_deref().and_then(en_marcha_de);
    }
    if quiere(Id::Procesos) {
        m.procesos = procesos();
    }
    l.m = m;
}

// ------------------------------------------------------------- words

/// Bytes per second in the unit that keeps the larger of two values short.
pub fn tasa(a: f64, b: f64) -> String {
    let top = a.max(b);
    let (div, unit) = if top >= 1e6 {
        (1e6, "MB/s")
    } else if top >= 1e3 {
        (1e3, "KB/s")
    } else {
        (1.0, "B/s")
    };
    if div == 1.0 {
        format!("{:.0} / {:.0} {unit}", a, b)
    } else {
        format!("{:.1} / {:.1} {unit}", a / div, b / div)
    }
}

/// `3 d 07 h 12 m`, `7 h 12 m` or `12 m`.
pub fn duracion(s: u64) -> String {
    let (d, h, m) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60);
    if d > 0 {
        format!("{d} d {h:02} h {m:02} m")
    } else if h > 0 {
        format!("{h} h {m:02} m")
    } else {
        format!("{m} m")
    }
}

fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}

/// The rows to draw: the readings asked for that this machine can give.
pub fn filas(m: &Muestra, ids: &[Id], lang: Lang) -> Vec<Fila> {
    let es = lang == Lang::Es;
    let w = |a: &str, b: &str| if es { a.to_string() } else { b.to_string() };
    let fila = |etiqueta: String, valor: String| Fila {
        etiqueta,
        valor,
        barra: None,
        traza: None,
        aviso: false,
    };
    ids.iter()
        .filter_map(|id| match id {
            Id::Carga => m.carga.map(|c| {
                fila(
                    w("carga 1/5/15", "load 1/5/15"),
                    format!("{:.2}  {:.2}  {:.2}", c[0], c[1], c[2]),
                )
            }),
            Id::Cpu => m.cpu.map(|(f, n)| Fila {
                barra: Some(f),
                aviso: f >= 0.9,
                ..fila(
                    if es {
                        format!("cpu · {n} hilos")
                    } else {
                        format!("cpu · {n} threads")
                    },
                    format!("{:.0} %", f * 100.0),
                )
            }),
            Id::Memoria => m.mem.map(|(u, t)| Fila {
                barra: Some(u as f32 / t as f32),
                aviso: u * 10 >= t * 9,
                ..fila(
                    w("memoria", "memory"),
                    format!("{:.1} / {:.1} GiB", gib(u << 10), gib(t << 10)),
                )
            }),
            Id::Red => m.red.map(|(rx, tx)| Fila {
                traza: Some(m.red_traza.clone()),
                ..fila(w("red ▼ / ▲", "net ▼ / ▲"), tasa(rx, tx))
            }),
            Id::Disco => m.disco.map(|(u, t)| {
                let p = u as f64 / t as f64;
                Fila {
                    barra: Some(p as f32),
                    aviso: p >= 0.9,
                    ..fila(
                        w("disco /", "disk /"),
                        if es {
                            format!("{:.0} % de {:.0} GiB", p * 100.0, gib(t))
                        } else {
                            format!("{:.0} % of {:.0} GiB", p * 100.0, gib(t))
                        },
                    )
                }
            }),
            Id::Temperatura => m.temp.map(|c| Fila {
                aviso: c >= 85,
                ..fila(w("temperatura", "temperature"), format!("{c} °C"))
            }),
            Id::EnMarcha => m
                .en_marcha
                .map(|s| fila(w("en marcha", "uptime"), duracion(s))),
            Id::Procesos => m
                .procesos
                .map(|n| fila(w("procesos", "processes"), n.to_string())),
        })
        .collect()
}

// ------------------------------------------------------------- drawing

type Mascara = (u32, u32, Vec<u8>);
type Cache = HashMap<(char, u32), Option<std::sync::Arc<Mascara>>>;

/// A character's coverage at `em` px, rasterised once per process.
fn mascara(ch: char, em: f32) -> Option<std::sync::Arc<Mascara>> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    let c = C.get_or_init(|| Mutex::new(HashMap::new()));
    let mut c = c.lock().unwrap_or_else(|p| p.into_inner());
    c.entry((ch, em.to_bits()))
        .or_insert_with(|| {
            let cell = lunartext::Cell {
                width: (em * 0.602).ceil(),
                em,
                baseline: (em * 0.8).round(),
            };
            let h = (em * 1.2).ceil() as u32;
            lunartext::coverage(ch, lunartext::Weight::Regular, cell, h).map(std::sync::Arc::new)
        })
        .clone()
}

/// Advance of one character at `em` px.
fn avance(em: f32) -> f32 {
    (em * 0.602).ceil()
}

impl PixBuf<'_> {
    /// `s` with its left edge at `x` and baseline at `base`; `extra` adds
    /// spacing between letters. The 5x7 face stands in when there is no
    /// vector one (`LUNAR_TEXT=bitmap`), in capitals, which is all it has.
    #[allow(clippy::too_many_arguments)]
    fn texto(&mut self, s: &str, x: f32, base: f32, em: f32, extra: f32, c: Rgb, a: f32) {
        let adv = avance(em) + extra;
        let top = (base - (em * 0.8).round()).round() as i32;
        for (i, ch) in s.chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            let gx = x + i as f32 * adv;
            match mascara(ch, em) {
                Some(m) => {
                    let (w, h, data) = (&m.0, &m.1, &m.2);
                    let left = gx.round() as i32;
                    for row in 0..*h as i32 {
                        for col in 0..*w as i32 {
                            let cov = data[(row * *w as i32 + col) as usize];
                            let (px, py) = (left + col, top + row);
                            if cov > 0 && px >= 0 && py >= 0 {
                                self.blend(px as usize, py as usize, c, a * cov as f32 / 255.0);
                            }
                        }
                    }
                }
                None => {
                    for up in ch.to_uppercase() {
                        self.glyph5x7(up, gx + adv / 2.0, base - 4.0, 1, c, a);
                    }
                }
            }
        }
    }

    fn ancho(s: &str, em: f32, extra: f32) -> f32 {
        s.chars().count() as f32 * (avance(em) + extra)
    }

    fn caja(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb, a: f32) {
        let (x0, y0) = (x.round().max(0.0) as usize, y.round().max(0.0) as usize);
        let (x1, y1) = (
            (x + w).round().max(0.0) as usize,
            (y + h).round().max(0.0) as usize,
        );
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.blend(xx, yy, c, a);
            }
        }
    }
}

/// Draw the column for the face at (`cx`, `cy`) with scale `s`.
#[allow(clippy::too_many_arguments)]
pub(super) fn dibujar(
    pb: &mut PixBuf,
    cx: f32,
    cy: f32,
    s: f32,
    lado: Lado,
    rows: &[Fila],
    cyan: Rgb,
    gold: Rgb,
    muted: Rgb,
) {
    let n = rows.len();
    if n == 0 {
        return;
    }
    let sx = pb.sx;
    let px = pb.px;
    let r = super::tecno::R_OUTER * s;
    let sign = if lado == Lado::Derecha { 1.0 } else { -1.0 };
    // The column: its dot at `col`, text running right from `x0`.
    let col = cx + sign * (super::tecno::R_OUTER * sx + COL_DX) * s;
    let x0 = if lado == Lado::Derecha {
        col
    } else {
        col - COL_W * s
    };
    let x_end = x0 + COL_W * s;
    let em = (EM * s).round().max(8.0);
    let mid = 0.36 * em; // from a row's centre line down to its baseline

    // Header, gold and letter-spaced, with a hairline under it.
    let top = cy + dy(0, n) * s - HEAD_DY * s;
    let head_x = if lado == Lado::Derecha {
        x0 + LABEL_DX * s
    } else {
        x0
    };
    pb.texto(
        "ECLIPSE · LECTURAS",
        head_x,
        top + mid,
        (11.0 * s).round(),
        2.0 * s,
        gold,
        0.95,
    );
    pb.caja(x0, top + 10.0 * s, COL_W * s, px, HAIR, 1.0);

    // The arc that joins the first and last ticks along the outer ring.
    let th = (dy(0, n) / super::tecno::R_OUTER).asin().to_degrees();
    // `th` is negative (the first row is above the centre): the arc runs
    // from -|th| to +|th| around 0° on the right, around 180° on the left.
    let a0 = if lado == Lado::Derecha {
        th
    } else {
        180.0 + th
    };
    pb.sweep(cx, cy, r, a0, -2.0 * th, px, cyan, 0.5);

    for (i, f) in rows.iter().enumerate() {
        let d = dy(i, n) * s;
        let y = cy + d;
        let Some(k) = sobre_anillo(r, d) else {
            continue;
        };
        let xr = cx + sign * k * sx;
        // The tick: radial, across the ring.
        let (ux, uy) = (sign * k * sx / r, d / r);
        pb.line(
            xr - ux * 5.0 * s,
            y - uy * 5.0 * s,
            xr + ux * 5.0 * s,
            y + uy * 5.0 * s,
            1.4 * px,
            cyan,
            0.95,
        );
        // The level hairline from the ring to the column, and its dot.
        pb.line(xr + sign * 6.0 * s, y, col, y, px, HAIR, 1.0);
        pb.dot(col, y, 2.0 * px, cyan, 1.0);

        let text_x = if lado == Lado::Derecha {
            x0 + LABEL_DX * s
        } else {
            x0
        };
        let val_end = if lado == Lado::Derecha {
            x_end
        } else {
            x_end - LABEL_DX * s
        };
        pb.texto(&f.etiqueta, text_x, y + mid, em, 0.0, muted, 0.95);
        let vw = PixBuf::ancho(&f.valor, em, 0.0);
        pb.texto(
            &f.valor,
            val_end - vw,
            y + mid,
            em,
            0.0,
            if f.aviso { WARN } else { TEXT },
            1.0,
        );

        let bx = text_x + BAR_DX * s
            - if lado == Lado::Derecha {
                0.0
            } else {
                LABEL_DX * s
            };
        if let Some(v) = f.barra {
            let lit = (v.clamp(0.0, 1.0) * SEGS as f32).round() as usize;
            for k in 0..SEGS {
                let on = k < lit;
                let c = if on {
                    if f.aviso {
                        WARN
                    } else {
                        cyan
                    }
                } else {
                    HAIR
                };
                pb.caja(
                    bx + k as f32 * 3.0 * s,
                    y - 4.5 * s,
                    2.0 * s,
                    9.0 * s,
                    c,
                    1.0,
                );
            }
        }
        if let Some(t) = &f.traza {
            let base = y + 5.0 * s;
            let mut prev: Option<(f32, f32)> = None;
            for (k, v) in t.iter().enumerate() {
                let p = (bx + k as f32 * s, base - v.clamp(0.0, 1.0) * 12.0 * s);
                if let Some(q) = prev {
                    // Steps: across, then up or down.
                    pb.line(q.0, q.1, p.0, q.1, px, cyan, 0.9);
                    pb.line(p.0, q.1, p.0, p.1, px, cyan, 0.9);
                }
                prev = Some(p);
            }
            if t.is_empty() {
                pb.line(bx, base, bx + TRAZA as f32 * s, base, px, HAIR, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reading_is_on_and_the_column_is_right_unless_a_clave_says_otherwise() {
        let a = ajustes_de(|_| None);
        assert_eq!(a.lado, Some(Lado::Derecha));
        assert_eq!(a.filas.len(), LECTURAS.len());
        assert!(a.reloj && a.fecha);
        let a = ajustes_de(|k| match k {
            "LUNARBG_LECTURAS" => Some("izquierda".into()),
            "LUNARBG_LECTURA_RED" | "LUNARBG_RELOJ" => Some("no".into()),
            // Not a word the clave has: the default holds.
            "LUNARBG_FECHA" => Some("quizá".into()),
            _ => None,
        });
        assert_eq!(a.lado, Some(Lado::Izquierda));
        assert!(!a.filas.contains(&Id::Red) && a.filas.len() == LECTURAS.len() - 1);
        assert!(!a.reloj && a.fecha);
        assert_eq!(
            ajustes_de(|k| (k == "LUNARBG_LECTURAS").then(|| "no".into())).lado,
            None
        );
    }

    #[test]
    fn the_rows_are_centred_a_pitch_apart_and_their_ticks_sit_on_the_ring() {
        assert_eq!(dy(0, 1), 0.0);
        assert_eq!(dy(0, 9), -128.0);
        assert_eq!(dy(8, 9), 128.0);
        assert_eq!(dy(1, 2) - dy(0, 2), PASO);
        let r = super::super::tecno::R_OUTER;
        for i in 0..9 {
            let d = dy(i, 9);
            let k = sobre_anillo(r, d).unwrap();
            assert!(((k * k + d * d).sqrt() - r).abs() < 1e-3);
        }
        assert_eq!(sobre_anillo(10.0, 11.0), None);
        // The angle of the first row of nine, as the mockup shows it.
        assert!(((dy(0, 9) / r).asin().to_degrees() + 22.84).abs() < 0.05);
    }

    #[test]
    fn the_column_lies_outside_the_ring_on_its_side() {
        let r = super::super::tecno::R_OUTER;
        let (a, b) = extension(Lado::Derecha, 720.0, 1.0, 1.0);
        assert!(a > 720.0 + r && b > a + COL_W);
        let (c, d) = extension(Lado::Izquierda, 720.0, 1.0, 1.0);
        assert!(d < 720.0 - r && d - c > COL_W);
        // Mirror images of each other.
        assert!(((720.0 - c) - (b - 720.0)).abs() < 1e-3);
    }

    #[test]
    fn the_proc_files_are_read_as_the_kernel_writes_them() {
        assert_eq!(
            carga_de("0.84 1.21 0.97 2/214 9001\n"),
            Some([0.84, 1.21, 0.97])
        );
        assert_eq!(carga_de("x"), None);
        let stat = "cpu  100 0 50 800 50 0 0 0\ncpu0 50 0 25 400 25 0 0 0\ncpu1 50 0 25 400 25 0 0 0\nintr 1\n";
        assert_eq!(cpu_de(stat), Some((150, 1000, 2)));
        assert_eq!(cpu_de("intr 1\n"), None);
        let mem = "MemTotal:       32000000 kB\nMemFree:  1000 kB\nMemAvailable:   20000000 kB\n";
        assert_eq!(mem_de(mem), Some((12_000_000, 32_000_000)));
        let dev = "Inter-|   Receive\n face |bytes\n    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  eth0: 1000 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0\n";
        assert_eq!(red_de(dev), Some((1000, 500)));
        assert_eq!(en_marcha_de("270732.51 1000.0\n"), Some(270_732));
    }

    #[test]
    fn values_read_short_and_a_missing_source_drops_its_row() {
        assert_eq!(tasa(2_400_000.0, 300_000.0), "2.4 / 0.3 MB/s");
        assert_eq!(tasa(800.0, 12.0), "800 / 12 B/s");
        assert_eq!(duracion(3 * 86_400 + 7 * 3600 + 12 * 60), "3 d 07 h 12 m");
        assert_eq!(duracion(7 * 3600 + 5 * 60), "7 h 05 m");
        assert_eq!(duracion(59), "0 m");
        let m = Muestra {
            carga: Some([1.0, 2.0, 3.0]),
            procesos: Some(214),
            ..Default::default()
        };
        let ids: Vec<Id> = LECTURAS.iter().map(|(i, _)| *i).collect();
        let f = filas(&m, &ids, Lang::Es);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].valor, "1.00  2.00  3.00");
        assert_eq!(f[1].etiqueta, "procesos");
        assert_eq!(filas(&m, &ids, Lang::En)[1].etiqueta, "processes");
    }
}
