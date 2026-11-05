//! lunarmon — the system monitor: what is running and what it costs, and the
//! services eclipse-init supervises.
//!
//! **Procesos**: the CPU of each core, memory, network and disk throughput
//! (`/proc/diskstats`), then every process with its CPU share, memory,
//! threads and CPU time. The ones with a window (the bar's map of them,
//! `$XDG_RUNTIME_DIR/lunar/desktops`) are listed first, under APLICACIONES.
//! Typing filters by name, command or PID; a click on a column sorts by it.
//! Supr (or TERMINAR) sends SIGTERM, Shift+Supr (or MATAR) SIGKILL.
//!
//! **Servicios**: eclipse-init's state file (`/run/eclipse-init/services`)
//! with each service's state, PID and starts, the detail and the end of its
//! log for the one selected, and INICIAR / REINICIAR / PARAR, which become a
//! request to init (`lunarbar::sysmon::request`). A stop lasts this boot.
//!
//! Ending something the desktop stands on (labwc, the bar, seatd, D-Bus)
//! asks for the same press again within four seconds.
//!
//! An ordinary xdg-shell window with a server-side frame, refreshed every
//! second. Tab switches between the two lists, Esc clears the filter or
//! closes. `--dump PATH:WxH[:servicios]` renders a made-up machine.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::i18n::Lang;
use lunarbar::keys::{
    key_char, KEY_BACKSPACE_WL, KEY_DOWN_WL, KEY_ESC_WL, KEY_HOME_WL, KEY_PGDN_WL, KEY_PGUP_WL,
    KEY_TAB_WL, KEY_UP_WL,
};
use lunarbar::proc::{self as lproc, map_shm_pool};
use lunarbar::sysinfo;
use lunarbar::sysmon::{self, Mem, ProcStat, Svc, Verb};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

// ── look ─────────────────────────────────────────────────────────────────────

const SURFACE: Rgb = (0x0a, 0x14, 0x23);
const WELL: Rgb = (0x06, 0x0d, 0x18);
const HAIRLINE: Rgb = (0x1d, 0x35, 0x50);
const TEXT: Rgb = (0xe6, 0xf1, 0xff);
const MUTED: Rgb = (0x8a, 0xa4, 0xc2);
const DIM: Rgb = (0x4a, 0x60, 0x7a);
const CYAN: Rgb = (0x38, 0xe1, 0xff);
const GOLD: Rgb = (0xf2, 0xc1, 0x4e);
const WARN: Rgb = (0xff, 0x8a, 0x5c);
const OK: Rgb = (0x6e, 0xe7, 0xa8);

const DEFAULT_W: u32 = 1180;
const DEFAULT_H: u32 = 760;
const MIN_W: i32 = 800;
const MIN_H: i32 = 560;
const PAD: i32 = 16;
const HEAD_H: i32 = 48;
const STRIP_H: i32 = 120;
const SEARCH_H: i32 = 44;
const COLS_H: i32 = 28;
const ROW_H: i32 = 28;
const FOOT_H: i32 = 56;
const BTN_W: i32 = 120;
const BTN_H: i32 = 32;
const TAB_W: i32 = 120;
const TAB_H: i32 = 28;
const BAR_H: i32 = 8;
const LOG_LINES: usize = 14;
const MAX_FILTER: usize = 32;

/// What the desktop stands on: ending it asks twice.
const CRITICAL: &[&str] = &[
    "labwc", "lunarbar", "seatd", "dbus-daemon", "dbus", "dbus-system", "eclipse-init", "Xorg", "xorg",
];

// ── what is shown ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Procs,
    Svcs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sort {
    Pid,
    Name,
    Cpu,
    Mem,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Row {
    pid: i32,
    name: String,
    cmd: String,
    cpu: f32,
    mem_kib: u64,
    threads: u32,
    state: char,
    ticks: u64,
    /// The window's title, for a process that has one.
    window: Option<String>,
}

/// One line of the process list as drawn.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    Section(&'static str),
    Proc(usize),
}

/// A press that is waiting for its second one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    Signal(i32, bool),
    Service(String, Verb),
}

#[derive(Clone, Debug, Default)]
struct Machine {
    cores: Vec<f32>,
    mem: Mem,
    net_down: f64,
    net_up: f64,
    iface: String,
    addr: String,
    disk_read: f64,
    disk_write: f64,
    disk_name: String,
    root_pct: Option<u32>,
    uptime: String,
    load: String,
}

/// The previous sample, for rates.
#[derive(Default)]
struct Prev {
    at: Option<Instant>,
    ticks: HashMap<i32, u64>,
    cpus: Vec<(u64, u64)>,
    disk: (u64, u64),
    net: (u64, u64),
}

/// The Spanish or the English of one text, by the desktop's language.
fn tr(l: Lang, es: &'static str, en: &'static str) -> &'static str {
    if l == Lang::En {
        en
    } else {
        es
    }
}

struct Mon {
    /// Spanish unless the desktop says English; the tests keep Spanish.
    lang: Lang,
    tab: Tab,
    m: Machine,
    rows: Vec<Row>,
    svcs: Vec<Svc>,
    log: Vec<String>,
    sort: Sort,
    filter: String,
    /// The process selected, by PID, so a refresh does not move it.
    sel_pid: Option<i32>,
    sel_svc: usize,
    top: usize,
    status: String,
    bad: bool,
    pending: Option<(Pending, Instant)>,
    now_wall: u64,
}

impl Mon {
    fn new() -> Self {
        Mon {
            lang: Lang::Es,
            tab: Tab::Procs,
            m: Machine::default(),
            rows: Vec::new(),
            svcs: Vec::new(),
            log: Vec::new(),
            sort: Sort::Cpu,
            filter: String::new(),
            sel_pid: None,
            sel_svc: 0,
            top: 0,
            status: String::new(),
            bad: false,
            pending: None,
            now_wall: 0,
        }
    }

    /// The process list as drawn: applications first, then the rest, each
    /// sorted, both filtered.
    fn items(&self) -> Vec<Item> {
        let f = self.filter.to_lowercase();
        let keep = |r: &Row| {
            f.is_empty()
                || r.name.to_lowercase().contains(&f)
                || r.cmd.to_lowercase().contains(&f)
                || r.pid.to_string() == f
        };
        let mut apps: Vec<usize> = Vec::new();
        let mut sys: Vec<usize> = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if keep(r) {
                if r.window.is_some() {
                    apps.push(i);
                } else {
                    sys.push(i);
                }
            }
        }
        let key = |i: &usize| {
            let r = &self.rows[*i];
            r.pid
        };
        let sort = |v: &mut Vec<usize>| match self.sort {
            Sort::Pid => v.sort_by_key(key),
            Sort::Name => v.sort_by(|a, b| self.rows[*a].name.to_lowercase().cmp(&self.rows[*b].name.to_lowercase())),
            Sort::Cpu => v.sort_by(|a, b| {
                self.rows[*b].cpu.total_cmp(&self.rows[*a].cpu).then(self.rows[*b].ticks.cmp(&self.rows[*a].ticks))
            }),
            Sort::Mem => v.sort_by(|a, b| self.rows[*b].mem_kib.cmp(&self.rows[*a].mem_kib)),
        };
        sort(&mut apps);
        sort(&mut sys);
        let mut out = Vec::new();
        if !apps.is_empty() {
            out.push(Item::Section(tr(self.lang, "APLICACIONES · con ventana", "APPLICATIONS · with a window")));
            out.extend(apps.into_iter().map(Item::Proc));
        }
        if !sys.is_empty() {
            out.push(Item::Section(tr(self.lang, "SISTEMA", "SYSTEM")));
            out.extend(sys.into_iter().map(Item::Proc));
        }
        out
    }

    fn selected_row(&self) -> Option<&Row> {
        let pid = self.sel_pid?;
        self.rows.iter().find(|r| r.pid == pid)
    }

    /// The processes the list shows, in order: what ↑/↓ walk.
    fn shown_pids(&self) -> Vec<i32> {
        self.items()
            .iter()
            .filter_map(|it| match it {
                Item::Proc(i) => Some(self.rows[*i].pid),
                _ => None,
            })
            .collect()
    }

    fn move_sel(&mut self, delta: i32, rows_visible: usize) {
        match self.tab {
            Tab::Procs => {
                let pids = self.shown_pids();
                if pids.is_empty() {
                    return;
                }
                let at = self.sel_pid.and_then(|p| pids.iter().position(|x| *x == p));
                let next = match at {
                    None => 0,
                    Some(a) => (a as i32 + delta).clamp(0, pids.len() as i32 - 1) as usize,
                };
                self.sel_pid = Some(pids[next]);
                self.keep_visible(rows_visible);
            }
            Tab::Svcs => {
                if !self.svcs.is_empty() {
                    self.sel_svc = (self.sel_svc as i32 + delta).clamp(0, self.svcs.len() as i32 - 1) as usize;
                    self.reload_log();
                }
            }
        }
    }

    /// Scroll so the selected process is on screen.
    fn keep_visible(&mut self, n: usize) {
        let items = self.items();
        let Some(pos) = items.iter().position(|it| matches!(it, Item::Proc(i) if Some(self.rows[*i].pid) == self.sel_pid)) else {
            return;
        };
        if pos < self.top {
            self.top = pos.saturating_sub(1);
        } else if pos >= self.top + n {
            self.top = pos + 1 - n;
        }
    }

    fn scroll(&mut self, delta: i32, n: usize) {
        let max = self.items().len().saturating_sub(n);
        self.top = (self.top as i32 + delta).clamp(0, max as i32) as usize;
    }

    fn reload_log(&mut self) {
        self.log = self
            .svcs
            .get(self.sel_svc)
            .and_then(|s| s.log.clone())
            .map(|p| sysmon::log_tail(Path::new(&p), LOG_LINES))
            .unwrap_or_default();
    }

    /// Ask for `what`; a critical target needs it asked twice within four
    /// seconds. `act` does it, passed in so the tests do nothing real.
    fn confirm_then(&mut self, what: Pending, critical: bool, act: &mut dyn FnMut(&Pending) -> std::io::Result<()>) {
        let fresh = self
            .pending
            .as_ref()
            .is_some_and(|(p, at)| *p == what && at.elapsed() < Duration::from_secs(4));
        if critical && !fresh {
            let who = target_name(self, &what);
            self.status = match self.lang {
                Lang::En => format!("{who} is part of the desktop: press again to confirm"),
                Lang::Es => format!("{who} es parte del escritorio: pulsa otra vez para confirmar"),
            };
            self.bad = true;
            self.pending = Some((what, Instant::now()));
            return;
        }
        self.pending = None;
        match act(&what) {
            Ok(()) => {
                self.status = done_text(self, &what);
                self.bad = false;
            }
            Err(e) => {
                self.status = format!("{}: {e}", tr(self.lang, "no se pudo", "could not"));
                self.bad = true;
            }
        }
    }

    fn signal_selected(&mut self, kill: bool, act: &mut dyn FnMut(&Pending) -> std::io::Result<()>) {
        let Some(r) = self.selected_row().cloned() else {
            self.status = tr(self.lang, "elige un proceso de la lista", "pick a process from the list").into();
            self.bad = true;
            return;
        };
        if r.pid == 1 {
            self.status = tr(self.lang, "eclipse-init no se termina desde aquí", "eclipse-init is not ended from here").into();
            self.bad = true;
            return;
        }
        let critical = CRITICAL.contains(&r.name.as_str());
        self.confirm_then(Pending::Signal(r.pid, kill), critical, act);
    }

    fn service_verb(&mut self, verb: Verb, act: &mut dyn FnMut(&Pending) -> std::io::Result<()>) {
        let Some(s) = self.svcs.get(self.sel_svc).cloned() else { return };
        if s.kind != "respawn" {
            self.status = match self.lang {
                Lang::En => format!("{} runs once at boot: it is not started or stopped", s.name),
                Lang::Es => format!("{} se ejecuta una vez al arrancar: no se inicia ni se para", s.name),
            };
            self.bad = true;
            return;
        }
        // Stopping or restarting what the desktop stands on asks twice;
        // starting it does not.
        let critical = verb != Verb::Start && CRITICAL.contains(&s.name.as_str());
        self.confirm_then(Pending::Service(s.name.clone(), verb), critical, act);
    }

    // ── sampling ──

    /// Fold one sample of the machine in. Texts, not paths, so a test and the
    /// dump can feed it a machine of their own.
    #[allow(clippy::too_many_arguments)]
    fn sample(
        &mut self,
        prev: &mut Prev,
        now: Instant,
        stat: &str,
        meminfo: &str,
        netdev: &str,
        diskstats: &str,
        procs: Vec<(ProcStat, String)>,
        windows: &[(String, String)],
        services: &str,
    ) {
        let dt = prev.at.map(|t| now.saturating_duration_since(t).as_secs_f64()).unwrap_or(0.0);
        let cpus = sysmon::parse_cpus(stat);
        self.m.cores = if prev.cpus.len() == cpus.len() { sysmon::core_load(&prev.cpus, &cpus) } else { vec![0.0; cpus.len()] };
        let ncpu = cpus.len().max(1);
        prev.cpus = cpus;
        self.m.mem = sysmon::parse_meminfo(meminfo);

        let net = sysinfo::net_totals_from(netdev);
        let disks = sysmon::parse_diskstats(diskstats);
        let disk = disks.iter().fold((0, 0), |a, d| (a.0 + d.1, a.1 + d.2));
        if dt > 0.0 {
            self.m.net_down = net.0.saturating_sub(prev.net.0) as f64 / dt;
            self.m.net_up = net.1.saturating_sub(prev.net.1) as f64 / dt;
            self.m.disk_read = disk.0.saturating_sub(prev.disk.0) as f64 * 512.0 / dt;
            self.m.disk_write = disk.1.saturating_sub(prev.disk.1) as f64 * 512.0 / dt;
        }
        prev.net = net;
        prev.disk = disk;
        self.m.disk_name = match disks.len() {
            0 => String::new(),
            1 => disks[0].0.clone(),
            n => format!("{} {}", n, tr(self.lang, "discos", "disks")),
        };

        let mut ticks = HashMap::with_capacity(procs.len());
        self.rows = procs
            .into_iter()
            .map(|(p, cmd)| {
                let d = prev.ticks.get(&p.pid).map(|t| p.ticks.saturating_sub(*t)).unwrap_or(0);
                ticks.insert(p.pid, p.ticks);
                let window = windows.iter().find(|(a, _)| sysmon::app_matches(a, &p.comm)).map(|(_, t)| t.clone());
                Row {
                    pid: p.pid,
                    name: p.comm,
                    cmd,
                    cpu: if dt > 0.0 { sysmon::cpu_share(d, dt, ncpu) } else { 0.0 },
                    mem_kib: p.rss * sysmon::PAGE_KIB,
                    threads: p.threads,
                    state: p.state,
                    ticks: p.ticks,
                    window,
                }
            })
            .collect();
        prev.ticks = ticks;
        prev.at = Some(now);
        if self.sel_pid.is_some_and(|p| !self.rows.iter().any(|r| r.pid == p)) {
            self.sel_pid = None;
        }

        let name = self.svcs.get(self.sel_svc).map(|s| s.name.clone());
        self.svcs = sysmon::parse_services(services);
        if let Some(n) = name {
            self.sel_svc = self.svcs.iter().position(|s| s.name == n).unwrap_or(0);
        }
        self.sel_svc = self.sel_svc.min(self.svcs.len().saturating_sub(1));
    }

    fn sample_live(&mut self, prev: &mut Prev) {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/0".into());
        let windows = sysmon::parse_windows(&read(&format!("{dir}/lunar/desktops")));
        self.sample(
            prev,
            Instant::now(),
            &read("/proc/stat"),
            &read("/proc/meminfo"),
            &read("/proc/net/dev"),
            &read("/proc/diskstats"),
            sysmon::processes(Path::new("/proc")),
            &windows,
            &read(sysmon::SERVICES_FILE),
        );
        self.m.root_pct = sysinfo::disk_root_percent();
        self.m.uptime = read("/proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| sysmon::fmt_secs(s as u64))
            .unwrap_or_default();
        self.m.load = read("/proc/loadavg").split_whitespace().take(3).collect::<Vec<_>>().join(" ").replace('.', ",");
        let (iface, addr) = first_iface();
        self.m.iface = iface;
        self.m.addr = addr;
        self.now_wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.reload_log();
    }
}

fn target_name(m: &Mon, p: &Pending) -> String {
    match p {
        Pending::Signal(pid, _) => m.rows.iter().find(|r| r.pid == *pid).map(|r| r.name.clone()).unwrap_or(pid.to_string()),
        Pending::Service(n, _) => n.clone(),
    }
}

fn done_text(m: &Mon, p: &Pending) -> String {
    let who = target_name(m, p);
    let l = m.lang;
    match p {
        Pending::Signal(pid, false) => format!("{who} ({pid}): {}", tr(l, "pedido que termine", "asked to end")),
        Pending::Signal(pid, true) => format!("{who} ({pid}): {}", tr(l, "terminado a la fuerza", "killed")),
        Pending::Service(_, Verb::Start) => format!("{who}: {}", tr(l, "pedido a eclipse-init que lo inicie", "eclipse-init asked to start it")),
        Pending::Service(_, Verb::Stop) => format!("{who}: {}", tr(l, "parado hasta que se inicie o se reinicie el equipo", "stopped until started again or the next boot")),
        Pending::Service(_, Verb::Restart) => format!("{who}: {}", tr(l, "reiniciando", "restarting")),
    }
}

fn act_for_real(p: &Pending) -> std::io::Result<()> {
    match p {
        Pending::Signal(pid, kill) => sysmon::signal(*pid, *kill),
        Pending::Service(name, verb) => sysmon::request(*verb, name),
    }
}

/// The first interface that is up and has an address, for the RED plate.
fn first_iface() -> (String, String) {
    let out = std::process::Command::new("ip")
        .args(["-o", "-4", "addr"])
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    lunarbar::netconf::parse_ip_addr(&out)
        .into_iter()
        .find(|(n, _, _)| n != "lo")
        .map(|(n, _, a)| (n, a))
        .unwrap_or_default()
}

// ── layout ───────────────────────────────────────────────────────────────────

type Rect = (i32, i32, i32, i32);

fn hit(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

fn tabs(w: i32) -> [Rect; 2] {
    let _ = w;
    let x = PAD + 96;
    let y = (HEAD_H - TAB_H) / 2;
    [(x, y, TAB_W, TAB_H), (x + TAB_W, y, TAB_W, TAB_H)]
}

/// The top of the process rows, and how many fit.
fn proc_rows(h: i32) -> (i32, usize) {
    let top = HEAD_H + STRIP_H + SEARCH_H + COLS_H;
    let room = h - top - FOOT_H;
    (top, (room / ROW_H).max(1) as usize)
}

/// Column right edges of the process table, from the right: TIEMPO, ESTADO,
/// HILOS, MEMORIA, CPU. The name takes what is left after PID.
fn columns(w: i32) -> [i32; 5] {
    let r = w - PAD;
    [r, r - 112, r - 112 - 72, r - 112 - 72 - 80, r - 112 - 72 - 80 - 112]
}

const PID_W: i32 = 72;

fn foot_buttons(w: i32, h: i32) -> [Rect; 3] {
    let y = h - FOOT_H + (FOOT_H - BTN_H) / 2;
    let x3 = w - PAD - BTN_W;
    [(x3 - 2 * (BTN_W + 8), y, BTN_W, BTN_H), (x3 - BTN_W - 8, y, BTN_W, BTN_H), (x3, y, BTN_W, BTN_H)]
}

fn svc_list_w(w: i32) -> i32 {
    ((w - 2 * PAD) * 56 / 100).clamp(520, 720)
}

// ── drawing ──────────────────────────────────────────────────────────────────

fn fit(s: &str, px: i32) -> String {
    let cols = (px / GLYPH_W).max(1) as usize;
    if s.chars().count() <= cols {
        return s.to_string();
    }
    let mut t: String = s.chars().take(cols.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn frame(cv: &mut Canvas, r: Rect, c: Rgb) {
    let (x, y, w, h) = r;
    for (a, b, rw, rh) in [(x, y, w, 1), (x, y + h - 1, w, 1), (x, y, 1, h), (x + w - 1, y, 1, h)] {
        cv.fill_rect_a(a, b, rw, rh, c, 1.0);
    }
}

fn text_right(cv: &mut Canvas, s: &str, right: i32, y: i32, c: Rgb) {
    cv.text(s, right - Canvas::text_width(s), y, c);
}

fn button(cv: &mut Canvas, r: Rect, label: &str, col: Rgb, lit: bool) {
    cv.fill_rect_a(r.0, r.1, r.2, r.3, if lit { col } else { WELL }, if lit { 0.14 } else { 1.0 });
    frame(cv, r, if lit || col != MUTED { col } else { HAIRLINE });
    let tx = r.0 + (r.2 - Canvas::text_width(label)) / 2;
    let ty = r.1 + (r.3 - GLYPH_H) / 2;
    if lit {
        cv.text_bold(label, tx, ty, col);
    } else {
        cv.text(label, tx, ty, col);
    }
}

fn bar(cv: &mut Canvas, x: i32, y: i32, w: i32, frac: f32, c: Rgb) {
    cv.fill_rect_a(x, y, w, BAR_H, WELL, 1.0);
    frame(cv, (x, y, w, BAR_H), HAIRLINE);
    let f = ((w - 2) as f32 * frac.clamp(0.0, 1.0)) as i32;
    if f > 0 {
        cv.fill_rect_a(x + 1, y + 1, f, BAR_H - 2, c, 1.0);
    }
}

fn draw(cv: &mut Canvas, w: i32, h: i32, m: &Mon) {
    cv.fill_rect_a(0, 0, w, h, SURFACE, 1.0);
    // Head.
    let hy = (HEAD_H - GLYPH_H) / 2;
    cv.text_bold("MONITOR", PAD, hy, CYAN);
    for (k, (r, label)) in tabs(w).iter().zip([tr(m.lang, "PROCESOS", "PROCESSES"), tr(m.lang, "SERVICIOS", "SERVICES")]).enumerate() {
        let on = (k == 0) == (m.tab == Tab::Procs);
        cv.fill_rect_a(r.0, r.1, r.2, r.3, if on { CYAN } else { WELL }, if on { 0.16 } else { 1.0 });
        frame(cv, *r, if on { CYAN } else { HAIRLINE });
        let tx = r.0 + (r.2 - Canvas::text_width(label)) / 2;
        let ty = r.1 + (r.3 - GLYPH_H) / 2;
        if on {
            cv.text_bold(label, tx, ty, CYAN);
        } else {
            cv.text(label, tx, ty, MUTED);
        }
    }
    let summary = match m.tab {
        Tab::Procs => {
            let mut s = format!("{} {}", tr(m.lang, "encendido", "up"), m.m.uptime);
            if !m.m.load.is_empty() {
                s += &format!(" · {} {}", tr(m.lang, "carga", "load"), m.m.load);
            }
            s
        }
        Tab::Svcs => {
            let run = m.svcs.iter().filter(|s| s.state == "running").count();
            let bad = m.svcs.iter().filter(|s| sysmon::state_word(&s.state, m.lang).1).count();
            let mut s = format!("eclipse-init · {} {} · {run} {}", m.svcs.len(), tr(m.lang, "servicios", "services"), tr(m.lang, "en marcha", "running"));
            if bad > 0 {
                s += &format!(" · {bad} {}", tr(m.lang, "con problemas", "in trouble"));
            }
            s
        }
    };
    let room = w - (tabs(w)[1].0 + TAB_W) - 2 * PAD;
    let summary = fit(&summary, room);
    text_right(cv, &summary, w - PAD, hy, MUTED);
    cv.hline(0, HEAD_H - 1, w, HAIRLINE, 1.0);

    match m.tab {
        Tab::Procs => draw_procs(cv, w, h, m),
        Tab::Svcs => draw_svcs(cv, w, h, m),
    }
}

fn plate_head(cv: &mut Canvas, x: i32, y: i32, w: i32, title: &str, value: &str) {
    cv.text_bold(title, x, y, CYAN);
    text_right(cv, value, x + w, y, TEXT);
}

fn draw_strip(cv: &mut Canvas, w: i32, m: &Mon) {
    let top = HEAD_H + PAD;
    let gap = PAD;
    let pw = (w - 2 * PAD - 3 * gap) / 4;
    let xs: Vec<i32> = (0..4).map(|k| PAD + k * (pw + gap)).collect();
    let line = GLYPH_H + 8;

    // CPU: one bar per core, up to four rows; more cores share a row.
    let total = if m.m.cores.is_empty() { 0.0 } else { m.m.cores.iter().sum::<f32>() / m.m.cores.len() as f32 };
    plate_head(cv, xs[0], top, pw, "CPU", &format!("{} %", total.round() as u32));
    let rows = m.m.cores.len().clamp(1, 4);
    let per = m.m.cores.len().div_ceil(rows).max(1);
    for r in 0..rows {
        let y = top + line + r as i32 * (line - 4);
        let cores: Vec<f32> = m.m.cores.iter().skip(r * per).take(per).copied().collect();
        if cores.is_empty() {
            break;
        }
        let avg = cores.iter().sum::<f32>() / cores.len() as f32;
        let label = if per == 1 { format!("c{}", r) } else { format!("c{}-{}", r * per, r * per + cores.len() - 1) };
        cv.text(&label, xs[0], y, MUTED);
        let lw = 6 * GLYPH_W;
        bar(cv, xs[0] + lw, y + (GLYPH_H - BAR_H) / 2, pw - lw - 6 * GLYPH_W, avg / 100.0, CYAN);
        text_right(cv, &format!("{} %", avg.round() as u32), xs[0] + pw, y, MUTED);
    }

    // Memory.
    let mem = &m.m.mem;
    plate_head(cv, xs[1], top, pw, tr(m.lang, "MEMORIA", "MEMORY"), &format!("{} / {}", sysmon::fmt_kib(mem.used()), sysmon::fmt_kib(mem.total)));
    let frac = if mem.total > 0 { mem.used() as f32 / mem.total as f32 } else { 0.0 };
    bar(cv, xs[1], top + line + (GLYPH_H - BAR_H) / 2, pw, frac, CYAN);
    cv.text(&fit(&format!("{} {} · {} {}", tr(m.lang, "en caché", "cached"), sysmon::fmt_kib(mem.cached), tr(m.lang, "libre", "free"), sysmon::fmt_kib(mem.free)), pw), xs[1], top + 2 * line, MUTED);
    let swap = if mem.swap_total > 0 {
        format!("{} {} / {}", tr(m.lang, "intercambio", "swap"), sysmon::fmt_kib(mem.swap_total - mem.swap_free), sysmon::fmt_kib(mem.swap_total))
    } else {
        tr(m.lang, "sin intercambio", "no swap").into()
    };
    cv.text(&fit(&swap, pw), xs[1], top + 3 * line, MUTED);

    // Network.
    plate_head(cv, xs[2], top, pw, tr(m.lang, "RED", "NETWORK"), if m.m.iface.is_empty() { "—" } else { &m.m.iface });
    cv.text(tr(m.lang, "▼ recibe", "▼ in"), xs[2], top + line, MUTED);
    text_right(cv, &format!("{}/s", sysinfo::fmt_rate(m.m.net_down)), xs[2] + pw, top + line, TEXT);
    cv.text(tr(m.lang, "▲ envía", "▲ out"), xs[2], top + 2 * line, MUTED);
    text_right(cv, &format!("{}/s", sysinfo::fmt_rate(m.m.net_up)), xs[2] + pw, top + 2 * line, TEXT);
    cv.text(&fit(if m.m.addr.is_empty() { tr(m.lang, "sin dirección IPv4", "no IPv4 address") } else { &m.m.addr }, pw), xs[2], top + 3 * line, MUTED);

    // Disk.
    let root = m.m.root_pct.map(|p| format!("/ {p} %")).unwrap_or_default();
    plate_head(cv, xs[3], top, pw, tr(m.lang, "DISCO", "DISK"), &root);
    if m.m.disk_name.is_empty() {
        cv.text(&fit(tr(m.lang, "el kernel no da /proc/diskstats", "the kernel gives no /proc/diskstats"), pw), xs[3], top + line, DIM);
    } else {
        cv.text(tr(m.lang, "lee", "read"), xs[3], top + line, MUTED);
        text_right(cv, &format!("{}/s", sysinfo::fmt_rate(m.m.disk_read)), xs[3] + pw, top + line, TEXT);
        cv.text(tr(m.lang, "escribe", "write"), xs[3], top + 2 * line, MUTED);
        text_right(cv, &format!("{}/s", sysinfo::fmt_rate(m.m.disk_write)), xs[3] + pw, top + 2 * line, TEXT);
        cv.text(&fit(&m.m.disk_name, pw), xs[3], top + 3 * line, MUTED);
    }
    cv.hline(0, HEAD_H + STRIP_H - 1, w, HAIRLINE, 1.0);
}

fn draw_procs(cv: &mut Canvas, w: i32, h: i32, m: &Mon) {
    draw_strip(cv, w, m);
    // Search.
    let sy = HEAD_H + STRIP_H;
    let ty = sy + (SEARCH_H - GLYPH_H) / 2;
    let lw = cv.text(tr(m.lang, "Buscar", "Search"), PAD, ty, MUTED);
    let field = (PAD + lw + 12, sy + (SEARCH_H - 28) / 2, 264, 28);
    cv.fill_rect_a(field.0, field.1, field.2, field.3, WELL, 1.0);
    frame(cv, field, if m.filter.is_empty() { HAIRLINE } else { CYAN });
    if m.filter.is_empty() {
        cv.text(tr(m.lang, "escribe un nombre o un PID", "type a name or a PID"), field.0 + 8, ty, DIM);
    } else {
        let adv = cv.text(&m.filter, field.0 + 8, ty, TEXT);
        cv.fill_rect_a(field.0 + 8 + adv + 1, field.1 + 6, 2, 16, CYAN, 1.0);
    }
    let sort = match m.sort {
        Sort::Pid => "PID",
        Sort::Name => tr(m.lang, "nombre", "name"),
        Sort::Cpu => "CPU",
        Sort::Mem => tr(m.lang, "memoria", "memory"),
    };
    cv.text(&format!("{}: {sort}", tr(m.lang, "orden", "sort")), field.0 + field.2 + 16, ty, MUTED);
    let apps = m.rows.iter().filter(|r| r.window.is_some()).count();
    text_right(cv, &format!("{} {} · {apps} {}", m.rows.len(), tr(m.lang, "procesos", "processes"), tr(m.lang, "con ventana", "with a window")), w - PAD, ty, MUTED);

    // Column heads.
    let cy = sy + SEARCH_H;
    let c = columns(w);
    let hy = cy + (COLS_H - GLYPH_H) / 2;
    let head = |s: Sort| if m.sort == s { CYAN } else { MUTED };
    cv.text("PID", PAD, hy, head(Sort::Pid));
    cv.text(tr(m.lang, "NOMBRE", "NAME"), PAD + PID_W, hy, head(Sort::Name));
    text_right(cv, "CPU", c[4], hy, head(Sort::Cpu));
    text_right(cv, tr(m.lang, "MEMORIA", "MEMORY"), c[3], hy, head(Sort::Mem));
    text_right(cv, tr(m.lang, "HILOS", "THREADS"), c[2], hy, MUTED);
    text_right(cv, tr(m.lang, "ESTADO", "STATE"), c[1], hy, MUTED);
    text_right(cv, tr(m.lang, "TIEMPO", "TIME"), c[0], hy, MUTED);
    cv.hline(PAD, cy + COLS_H - 1, w - 2 * PAD, HAIRLINE, 1.0);

    // Rows.
    let (top, n) = proc_rows(h);
    let items = m.items();
    let name_w = c[4] - 112 + 40 - (PAD + PID_W) - 16;
    for (k, it) in items.iter().skip(m.top).take(n).enumerate() {
        let y = top + k as i32 * ROW_H;
        let ty = y + (ROW_H - GLYPH_H) / 2;
        match it {
            Item::Section(s) => {
                cv.text(s, PAD, ty + 2, GOLD);
            }
            Item::Proc(i) => {
                let r = &m.rows[*i];
                if m.sel_pid == Some(r.pid) {
                    cv.fill_rect_a(0, y, w, ROW_H, CYAN, 0.08);
                    cv.fill_rect_a(0, y + 4, 2, ROW_H - 8, CYAN, 1.0);
                }
                cv.text(&r.pid.to_string(), PAD, ty, MUTED);
                let adv = cv.text(&fit(&r.name, name_w), PAD + PID_W, ty, TEXT);
                if let Some(t) = &r.window {
                    let room = name_w - adv - 16;
                    if room > 4 * GLYPH_W && !t.is_empty() {
                        cv.text(&fit(&format!("· {t}"), room), PAD + PID_W + adv + 8, ty, DIM);
                    }
                }
                let cpu = format!("{:.1} %", r.cpu).replace('.', ",");
                text_right(cv, &cpu, c[4], ty, if r.cpu >= 50.0 { GOLD } else { TEXT });
                text_right(cv, &sysmon::fmt_kib(r.mem_kib), c[3], ty, TEXT);
                text_right(cv, &r.threads.to_string(), c[2], ty, MUTED);
                text_right(cv, &r.state.to_string(), c[1], ty, if r.state == 'Z' || r.state == 'D' { WARN } else { MUTED });
                text_right(cv, &sysmon::fmt_ticks(r.ticks), c[0], ty, MUTED);
            }
        }
    }
    if items.is_empty() {
        cv.text(tr(m.lang, "ningún proceso coincide", "no process matches"), PAD, top + 8, MUTED);
    }

    // Foot.
    let fy = h - FOOT_H;
    cv.hline(0, fy, w, HAIRLINE, 1.0);
    let b = foot_buttons(w, h);
    let info = match m.selected_row() {
        Some(r) => format!("{} · PID {} · {}", r.name, r.pid, if r.cmd.is_empty() { "—" } else { &r.cmd }),
        None => tr(m.lang, "↑↓ elegir · Supr terminar · Mayús+Supr matar · Tab servicios", "↑↓ pick · Del end · Shift+Del kill · Tab services").into(),
    };
    let text = if m.status.is_empty() { info } else { m.status.clone() };
    let col = if m.status.is_empty() { MUTED } else if m.bad { WARN } else { CYAN };
    cv.text(&fit(&text, b[0].0 - 2 * PAD), PAD, fy + (FOOT_H - GLYPH_H) / 2, col);
    let sel = m.selected_row().is_some();
    button(cv, b[1], tr(m.lang, "TERMINAR", "END"), if sel { GOLD } else { MUTED }, sel);
    button(cv, b[2], tr(m.lang, "MATAR", "KILL"), if sel { WARN } else { MUTED }, false);
}

fn draw_svcs(cv: &mut Canvas, w: i32, h: i32, m: &Mon) {
    let lw = svc_list_w(w);
    let top = HEAD_H;
    let body_h = h - HEAD_H - FOOT_H;
    cv.fill_rect_a(PAD + lw, top, 1, body_h, HAIRLINE, 1.0);
    // Columns: SERVICIO, TIPO, ESTADO, PID, ARRANQUES, DESDE.
    let x0 = PAD;
    let cols = [x0, x0 + 168, x0 + 168 + 104, x0 + lw - 216, x0 + lw - 120, x0 + lw - 8];
    let hy = top + (COLS_H - GLYPH_H) / 2 + 4;
    cv.text(tr(m.lang, "SERVICIO", "SERVICE"), cols[0], hy, MUTED);
    cv.text(tr(m.lang, "TIPO", "KIND"), cols[1], hy, MUTED);
    cv.text(tr(m.lang, "ESTADO", "STATE"), cols[2], hy, MUTED);
    text_right(cv, "PID", cols[3], hy, MUTED);
    text_right(cv, tr(m.lang, "ARRANQUES", "STARTS"), cols[4], hy, MUTED);
    text_right(cv, tr(m.lang, "DESDE", "FOR"), cols[5], hy, MUTED);
    let ry = top + COLS_H + 8;
    cv.hline(PAD, ry - 1, lw - 8, HAIRLINE, 1.0);
    let n = ((body_h - COLS_H - 8) / ROW_H).max(1) as usize;
    let first = m.sel_svc.saturating_sub(n.saturating_sub(1));
    if m.svcs.is_empty() {
        cv.text(&fit(tr(m.lang, "eclipse-init no ha escrito /run/eclipse-init/services", "eclipse-init has not written /run/eclipse-init/services"), lw - 16), PAD, ry + 8, MUTED);
    }
    for (k, s) in m.svcs.iter().enumerate().skip(first).take(n) {
        let y = ry + (k - first) as i32 * ROW_H;
        let ty = y + (ROW_H - GLYPH_H) / 2;
        if k == m.sel_svc {
            cv.fill_rect_a(0, y, PAD + lw, ROW_H, CYAN, 0.08);
            cv.fill_rect_a(0, y + 4, 2, ROW_H - 8, CYAN, 1.0);
        }
        cv.text(&fit(&s.name, 160), cols[0], ty, TEXT);
        cv.text(if s.kind == "respawn" { tr(m.lang, "continuo", "respawn") } else { tr(m.lang, "una vez", "once") }, cols[1], ty, MUTED);
        let (word, bad) = sysmon::state_word(&s.state, m.lang);
        let col = state_col(&s.state, bad);
        cv.fill_rect_a(cols[2], ty + (GLYPH_H - 6) / 2, 6, 6, col, 1.0);
        cv.text(word, cols[2] + 12, ty, col);
        text_right(cv, &s.pid.map(|p| p.to_string()).unwrap_or("—".into()), cols[3], ty, MUTED);
        text_right(cv, &s.starts.to_string(), cols[4], ty, MUTED);
        let since = s.started.map(|t| sysmon::fmt_secs(m.now_wall.saturating_sub(t))).unwrap_or("—".into());
        text_right(cv, &since, cols[5], ty, MUTED);
    }

    // Detail.
    let dx = PAD + lw + PAD;
    let dw = w - dx - PAD;
    if let Some(s) = m.svcs.get(m.sel_svc) {
        let y = top + PAD;
        cv.text_bold(&s.name.to_uppercase(), dx, y, CYAN);
        let (word, bad) = sysmon::state_word(&s.state, m.lang);
        let st = match s.pid {
            Some(p) => format!("{word} · PID {p}"),
            None => word.to_string(),
        };
        text_right(cv, &st, dx + dw, y, state_col(&s.state, bad));
        let line = GLYPH_H + 8;
        let lab = 14 * GLYPH_W;
        let proc_cost = s
            .pid
            .and_then(|p| m.rows.iter().find(|r| r.pid == p))
            .map(|r| format!("{} · {}", format!("{:.1} %", r.cpu).replace('.', ","), sysmon::fmt_kib(r.mem_kib)))
            .unwrap_or("—".into());
        let kind = if s.kind == "respawn" { tr(m.lang, "continuo: se reinicia si termina", "respawn: restarted when it ends") } else { tr(m.lang, "una vez, al arrancar", "once, at boot") };
        for (k, (a, b)) in [
            (tr(m.lang, "ejecuta", "runs"), s.exec.as_str()),
            (tr(m.lang, "tipo", "kind"), kind),
            (tr(m.lang, "registro", "log"), s.log.as_deref().unwrap_or("—")),
            (tr(m.lang, "CPU · memoria", "CPU · memory"), proc_cost.as_str()),
        ]
        .iter()
        .enumerate()
        {
            let ly = y + (k as i32 + 1) * line + 4;
            cv.text(a, dx, ly, MUTED);
            cv.text(&fit(b, dw - lab), dx + lab, ly, TEXT);
        }
        let ly = y + 5 * line + 12;
        cv.text(tr(m.lang, "ÚLTIMAS LÍNEAS DEL REGISTRO", "LAST LINES OF THE LOG"), dx, ly, MUTED);
        cv.hline(dx, ly + GLYPH_H + 4, dw, HAIRLINE, 1.0);
        let lb = (dx, ly + GLYPH_H + 12, dw, h - FOOT_H - PAD - (ly + GLYPH_H + 12));
        cv.fill_rect_a(lb.0, lb.1, lb.2, lb.3, WELL, 1.0);
        frame(cv, lb, HAIRLINE);
        let fits = ((lb.3 - 16) / (GLYPH_H + 4)).max(0) as usize;
        if m.log.is_empty() {
            let why = if s.log.is_none() { tr(m.lang, "este servicio no guarda registro", "this service keeps no log") } else { tr(m.lang, "el registro está vacío", "the log is empty") };
            cv.text(why, lb.0 + 8, lb.1 + 8, DIM);
        }
        for (k, l) in m.log.iter().skip(m.log.len().saturating_sub(fits)).enumerate() {
            cv.text(&fit(l, lb.2 - 16), lb.0 + 8, lb.1 + 8 + k as i32 * (GLYPH_H + 4), MUTED);
        }
    }

    // Foot.
    let fy = h - FOOT_H;
    cv.hline(0, fy, w, HAIRLINE, 1.0);
    let b = foot_buttons(w, h);
    let text = if m.status.is_empty() {
        tr(m.lang, "↑↓ elegir · I iniciar · R reiniciar · P parar (hasta el próximo arranque) · Tab procesos", "↑↓ pick · I start · R restart · P stop (until the next boot) · Tab processes").to_string()
    } else {
        m.status.clone()
    };
    let col = if m.status.is_empty() { MUTED } else if m.bad { WARN } else { CYAN };
    cv.text(&fit(&text, b[0].0 - 2 * PAD), PAD, fy + (FOOT_H - GLYPH_H) / 2, col);
    let s = m.svcs.get(m.sel_svc);
    let ctl = s.is_some_and(|s| s.kind == "respawn");
    let running = s.is_some_and(|s| s.pid.is_some());
    button(cv, b[0], tr(m.lang, "INICIAR", "START"), if ctl && !running { CYAN } else { MUTED }, false);
    button(cv, b[1], tr(m.lang, "REINICIAR", "RESTART"), if ctl { GOLD } else { MUTED }, ctl);
    button(cv, b[2], tr(m.lang, "PARAR", "STOP"), if ctl && running { WARN } else { MUTED }, false);
}

fn state_col(state: &str, bad: bool) -> Rgb {
    match state {
        "running" => CYAN,
        "waiting" => GOLD,
        _ if bad => WARN,
        "idle" => OK,
        _ => MUTED,
    }
}

// ── input ────────────────────────────────────────────────────────────────────

/// Evdev code of Delete (Supr).
const KEY_DELETE: u32 = 111;
const KEY_END: u32 = 107;

impl Mon {
    /// One key. Returns false when the window should close.
    fn key(&mut self, code: u32, shift: bool, rows_visible: usize, act: &mut dyn FnMut(&Pending) -> std::io::Result<()>) -> bool {
        if code != KEY_DELETE {
            self.pending = None;
        }
        match code {
            KEY_ESC_WL => {
                if self.tab == Tab::Procs && !self.filter.is_empty() {
                    self.filter.clear();
                    self.top = 0;
                } else {
                    return false;
                }
            }
            KEY_TAB_WL => {
                self.tab = if self.tab == Tab::Procs { Tab::Svcs } else { Tab::Procs };
                self.status.clear();
            }
            KEY_UP_WL => self.move_sel(-1, rows_visible),
            KEY_DOWN_WL => self.move_sel(1, rows_visible),
            KEY_PGUP_WL => self.move_sel(-(rows_visible as i32), rows_visible),
            KEY_PGDN_WL => self.move_sel(rows_visible as i32, rows_visible),
            KEY_HOME_WL => self.move_sel(-100_000, rows_visible),
            KEY_END => self.move_sel(100_000, rows_visible),
            KEY_DELETE if self.tab == Tab::Procs => self.signal_selected(shift, act),
            KEY_BACKSPACE_WL if self.tab == Tab::Procs => {
                self.filter.pop();
                self.top = 0;
            }
            _ => match (self.tab, key_char(code)) {
                (Tab::Procs, Some(c)) if self.filter.chars().count() < MAX_FILTER => {
                    self.filter.push(c);
                    self.top = 0;
                }
                (Tab::Svcs, Some('i')) => self.service_verb(Verb::Start, act),
                (Tab::Svcs, Some('r')) => self.service_verb(Verb::Restart, act),
                (Tab::Svcs, Some('p')) => self.service_verb(Verb::Stop, act),
                _ => {}
            },
        }
        true
    }

    fn click(&mut self, w: i32, h: i32, x: i32, y: i32, act: &mut dyn FnMut(&Pending) -> std::io::Result<()>) {
        let t = tabs(w);
        if hit(t[0], x, y) {
            self.tab = Tab::Procs;
            self.status.clear();
            return;
        }
        if hit(t[1], x, y) {
            self.tab = Tab::Svcs;
            self.status.clear();
            return;
        }
        let b = foot_buttons(w, h);
        match self.tab {
            Tab::Procs => {
                if hit(b[1], x, y) {
                    return self.signal_selected(false, act);
                }
                if hit(b[2], x, y) {
                    return self.signal_selected(true, act);
                }
                let cy = HEAD_H + STRIP_H + SEARCH_H;
                if y >= cy && y < cy + COLS_H {
                    let c = columns(w);
                    self.sort = if x < PAD + PID_W {
                        Sort::Pid
                    } else if x < c[4] - 112 + 40 {
                        Sort::Name
                    } else if x < c[4] + 8 {
                        Sort::Cpu
                    } else if x < c[3] + 8 {
                        Sort::Mem
                    } else {
                        self.sort
                    };
                    self.top = 0;
                    return;
                }
                let (top, n) = proc_rows(h);
                if y >= top && y < top + n as i32 * ROW_H {
                    let k = self.top + ((y - top) / ROW_H) as usize;
                    if let Some(Item::Proc(i)) = self.items().get(k) {
                        self.sel_pid = Some(self.rows[*i].pid);
                        self.pending = None;
                    }
                }
            }
            Tab::Svcs => {
                for (r, v) in b.iter().zip([Verb::Start, Verb::Restart, Verb::Stop]) {
                    if hit(*r, x, y) {
                        return self.service_verb(v, act);
                    }
                }
                let lw = svc_list_w(w);
                let ry = HEAD_H + COLS_H + 8;
                let n = ((h - HEAD_H - FOOT_H - COLS_H - 8) / ROW_H).max(1) as usize;
                let first = self.sel_svc.saturating_sub(n.saturating_sub(1));
                if x < PAD + lw && y >= ry {
                    let k = first + ((y - ry) / ROW_H) as usize;
                    if k < self.svcs.len() {
                        self.sel_svc = k;
                        self.pending = None;
                        self.reload_log();
                    }
                }
            }
        }
    }
}

// ── wayland ──────────────────────────────────────────────────────────────────

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    deco_mgr: Option<ZxdgDecorationManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    decoration: Option<ZxdgToplevelDecorationV1>,
    pending: (i32, i32),
    w: u32,
    h: u32,
    map: *mut u8,
    len: usize,
    buffer: Option<wl_buffer::WlBuffer>,
    mon: Mon,
    prev: Prev,
    shift: bool,
    ptr: (f64, f64),
    done: bool,
}

impl State {
    fn rows_visible(&self) -> usize {
        proc_rows(self.h as i32).1
    }

    fn render(&mut self) {
        let (Some(surface), Some(buf)) = (self.surface.clone(), self.buffer.clone()) else { return };
        if self.map.is_null() {
            return;
        }
        let Some(mut cv) = Canvas::try_new(self.w as usize, self.h as usize) else { return };
        cv.set_square(true);
        draw(&mut cv, self.w as i32, self.h as i32, &self.mon);
        // SAFETY: `map` is the `len` bytes mapped for this surface's buffer.
        let data = unsafe { std::slice::from_raw_parts_mut(self.map, self.len) };
        if cv.blit_argb(data) {
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, self.w as i32, self.h as i32);
            surface.commit();
        }
    }

    fn configure(&mut self, qh: &QueueHandle<State>) {
        let Some(shm) = self.shm.clone() else { return };
        let (pw, ph) = self.pending;
        let w = if pw > 0 { pw as u32 } else { DEFAULT_W };
        let h = if ph > 0 { ph as u32 } else { DEFAULT_H };
        if fill_guard::check_buffer(w as usize, h as usize).is_err() {
            return;
        }
        if self.w != w || self.h != h {
            let Some(geom) = lproc::pool_geometry(w, h) else { return };
            let Some((map, fd)) = map_shm_pool(geom.frame_size, "lunarmon") else { return };
            if let Some(b) = self.buffer.take() {
                b.destroy();
            }
            if !self.map.is_null() {
                unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
            }
            let pool = shm.create_pool(fd.as_fd(), geom.frame_size as i32, qh, ());
            self.buffer = Some(pool.create_buffer(0, w as i32, h as i32, geom.stride as i32, wl_shm::Format::Argb8888, qh, ()));
            pool.destroy();
            self.map = map;
            self.len = geom.frame_size;
            self.w = w;
            self.h = h;
            let n = self.rows_visible();
            self.mon.keep_visible(n);
        }
        self.render();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => st.wm_base = Some(reg.bind(name, version.min(2), qh, ())),
                "zxdg_decoration_manager_v1" => st.deco_mgr = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(st: &mut Self, xs: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            st.configure(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(st: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                st.pending = (if width > 0 { width.max(MIN_W) } else { 0 }, if height > 0 { height.max(MIN_H) } else { 0 });
            }
            xdg_toplevel::Event::Close => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(st: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
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
    fn event(st: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => st.shift = mods_depressed & 1 != 0,
            wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => {
                let n = st.rows_visible();
                if !st.mon.key(key, st.shift, n, &mut act_for_real) {
                    st.done = true;
                }
                st.render();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(st: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match ev {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } | wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                st.ptr = (surface_x, surface_y)
            }
            wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => {
                let (x, y) = (st.ptr.0 as i32, st.ptr.1 as i32);
                st.mon.click(st.w as i32, st.h as i32, x, y, &mut act_for_real);
                st.render();
            }
            wl_pointer::Event::Axis { value, .. } => {
                let n = st.rows_visible();
                match st.mon.tab {
                    Tab::Procs => st.mon.scroll(if value > 0.0 { 3 } else { -3 }, n),
                    Tab::Svcs => st.mon.move_sel(if value > 0.0 { 1 } else { -1 }, n),
                }
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
wayland_client::delegate_noop!(State: ignore ZxdgDecorationManagerV1);
wayland_client::delegate_noop!(State: ignore ZxdgToplevelDecorationV1);

// ── main ─────────────────────────────────────────────────────────────────────

/// A made-up machine, for `--dump` and the tests: two samples a second apart.
fn sample_mon() -> Mon {
    let mut m = Mon::new();
    let mut prev = Prev::default();
    let p = |pid: i32, comm: &str, ticks: u64, threads: u32, rss_mib: u64, cmd: &str| {
        (
            ProcStat { pid, comm: comm.into(), state: 'S', ppid: 1, ticks, threads, rss: rss_mib * 256, start: 0 },
            cmd.to_string(),
        )
    };
    let procs = |k: u64| {
        vec![
            p(1, "eclipse-init", 100, 1, 2, "/sbin/init"),
            p(401, "lunarnet", 20, 1, 3, "/usr/bin/lunarnet --service"),
            p(433, "hunterd", 3300 + k, 6, 41, "/sbin/hunterd"),
            p(455, "ntpd", 50, 1, 1, "ntpd -n -p pool.ntp.org"),
            p(598, "pulseaudio", 1900 + k * 2, 3, 14, "pulseaudio --system"),
            p(612, "labwc", 16000 + k * 15, 4, 96, "labwc"),
            p(640, "lunarbar", 4700 + k * 4, 2, 22, "lunarbar"),
            p(1388, "foot", 200, 3, 24, "foot"),
            p(1412, "firefox", 25_100 + k * 57, 58, 612, "/usr/lib/firefox/firefox"),
            p(1450, "Web Content", 9000 + k * 30, 22, 310, "/usr/lib/firefox/firefox -contentproc"),
            p(1520, "lunarfile", 100, 2, 18, "lunarfile"),
            p(1601, "lunarnet", 2, 1, 9, "lunarnet"),
        ]
    };
    let stat = |k: u64| {
        format!(
            "cpu  0 0 0 0 0\ncpu0 {} 0 0 {} 0\ncpu1 {} 0 0 {} 0\ncpu2 {} 0 0 {} 0\ncpu3 {} 0 0 {} 0\n",
            k * 31, 1000 + k * 69, k * 18, 1000 + k * 82, k * 27, 1000 + k * 73, k * 14, 1000 + k * 86
        )
    };
    let mem = "MemTotal: 8178432 kB\nMemFree: 4508876 kB\nMemAvailable: 5976883 kB\nCached: 1468006 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
    let net = |k: u64| format!("Inter-|\n face |\n    lo: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n  eth0: {} 0 0 0 0 0 0 0 {} 0 0 0 0 0 0 0\n", k * 1_258_291, k * 86_016);
    let disk = |k: u64| format!("   3       0 sda {} 0 {} 0 {} 0 {} 0 0 0 0\n", k * 10, k * 6963, k * 3, k * 440);
    let windows = vec![("firefox".to_string(), "Inicio — Mozilla Firefox".to_string()), ("foot".to_string(), "~".to_string()), ("lunarfile".to_string(), "/root".to_string())];
    let svcs = "# x\nmounts\toneshot\tidle\t-\t1\t-\t-\t/usr/local/bin/eclipse-mounts\nudhcpc\trespawn\trunning\t401\t1\t1793854000\t-\t/usr/local/bin/eclipse-udhcpc\nntpd\trespawn\trunning\t455\t1\t1793854060\t/tmp/ntpd.log\t/usr/local/bin/eclipse-ntpd\ndbus\trespawn\trunning\t470\t1\t1793854000\t-\t/usr/local/bin/eclipse-dbus\nnotify\trespawn\trunning\t488\t1\t1793854000\t-\t/usr/local/bin/eclipse-notify\nseatd\trespawn\trunning\t502\t1\t1793854000\t-\t/usr/local/bin/eclipse-seatd\nlabwc\trespawn\trunning\t612\t1\t1793854000\t/tmp/labwc.log\t/usr/local/bin/labwc\npulseaudio\trespawn\trunning\t598\t2\t1793856000\t-\t/usr/local/bin/eclipse-pulseaudio\nhunterd\trespawn\trunning\t433\t1\t1793854000\t-\t/sbin/hunterd\nportal\trespawn\twaiting\t-\t4\t-\t-\t/usr/local/bin/eclipse-portal\nollama\trespawn\tgiven-up\t-\t20\t-\t/tmp/ollama.log\t/usr/local/bin/eclipse-ollama\nsshd\trespawn\tstopped\t-\t1\t-\t-\t/usr/sbin/sshd -D\n";
    let t0 = Instant::now();
    m.sample(&mut prev, t0, &stat(0), mem, &net(0), &disk(0), procs(0), &windows, svcs);
    m.sample(&mut prev, t0 + Duration::from_secs(1), &stat(1), mem, &net(1), &disk(1), procs(1), &windows, svcs);
    m.m.iface = "eth0".into();
    m.m.addr = "10.0.2.15/24".into();
    m.m.root_pct = Some(41);
    m.m.uptime = "3 h 12 min".into();
    m.m.load = "0,84 0,61 0,52".into();
    m.now_wall = 1_793_865_520;
    m.sel_pid = Some(1412);
    m.sel_svc = 2;
    m.log = vec![
        "ntpd: setting time to 2026-11-05 07:41:02".into(),
        "ntpd: reply from 162.159.200.1: offset:+0.0031".into(),
        "ntpd: reply from 162.159.200.123: offset:+0.0029".into(),
        "ntpd: adjusting clock by 0.003s".into(),
    ];
    m
}

fn dump(spec: &str) -> ! {
    let mut parts = spec.split(':');
    let path = parts.next().unwrap_or("lunarmon.raw");
    let (w, h) = parts
        .next()
        .and_then(|d| d.split_once('x'))
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((DEFAULT_W as usize, DEFAULT_H as usize));
    if fill_guard::check_buffer(w, h).is_err() {
        std::process::exit(1);
    }
    let mut m = sample_mon();
    m.lang = Lang::current();
    if parts.next() == Some("servicios") {
        m.tab = Tab::Svcs;
    }
    let mut cv = Canvas::try_new(w, h).unwrap_or_else(|| std::process::exit(1));
    cv.set_square(true);
    draw(&mut cv, w as i32, h as i32, &m);
    let mut buf = vec![0u8; w * h * 4];
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        eprintln!("lunarmon: could not write {path}");
        std::process::exit(1);
    }
    println!("lunarmon: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {}
        Some("--dump") => dump(&args.next().unwrap_or_else(|| "lunarmon.raw".into())),
        Some("--servicios") => {}
        Some(_) => {
            eprintln!("usage: lunarmon [--servicios | --dump PATH:WxH[:servicios]]");
            std::process::exit(2);
        }
    }
    let mut mon = Mon::new();
    mon.lang = Lang::current();
    if std::env::args().nth(1).as_deref() == Some("--servicios") {
        mon.tab = Tab::Svcs;
    }
    let mut prev = Prev::default();
    mon.sample_live(&mut prev);
    let mut st = State {
        compositor: None,
        shm: None,
        wm_base: None,
        deco_mgr: None,
        seat: None,
        keyboard: None,
        pointer: None,
        surface: None,
        xdg_surface: None,
        toplevel: None,
        decoration: None,
        pending: (0, 0),
        w: 0,
        h: 0,
        map: std::ptr::null_mut(),
        len: 0,
        buffer: None,
        mon,
        prev,
        shift: false,
        ptr: (0.0, 0.0),
        done: false,
    };
    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarmon: no Wayland compositor found");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    if queue.roundtrip(&mut st).is_err() {
        std::process::exit(1);
    }
    let (Some(comp), Some(wm)) = (st.compositor.clone(), st.wm_base.clone()) else {
        eprintln!("lunarmon: compositor lacks xdg-shell");
        std::process::exit(1);
    };
    let surface = comp.create_surface(&qh, ());
    let xs = wm.get_xdg_surface(&surface, &qh, ());
    let top = xs.get_toplevel(&qh, ());
    top.set_title(tr(Lang::current(), "Monitor del sistema", "System monitor").into());
    top.set_app_id("lunarmon".into());
    top.set_min_size(MIN_W, MIN_H);
    if let Some(mgr) = &st.deco_mgr {
        let d = mgr.get_toplevel_decoration(&top, &qh, ());
        d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        st.decoration = Some(d);
    }
    surface.commit();
    st.surface = Some(surface);
    st.xdg_surface = Some(xs);
    st.toplevel = Some(top);
    if queue.roundtrip(&mut st).is_err() {
        std::process::exit(1);
    }

    // Events, and a fresh sample every second.
    let mut last = Instant::now();
    while !st.done {
        let _ = conn.flush();
        let wait = Duration::from_secs(1).saturating_sub(last.elapsed()).as_millis() as i32;
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut pfd, 1, wait.max(1)) };
            if n > 0 && pfd.revents & libc::POLLIN != 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        if queue.dispatch_pending(&mut st).is_err() {
            break;
        }
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            let mut prev = std::mem::take(&mut st.prev);
            st.mon.sample_live(&mut prev);
            st.prev = prev;
            // A status line is news for a few seconds, not for ever.
            if st.mon.pending.as_ref().is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(4)) {
                st.mon.pending = None;
            }
            st.render();
        }
    }
    if let Some(d) = st.decoration.take() {
        d.destroy();
    }
    if let Some(t) = st.toplevel.take() {
        t.destroy();
    }
    if let Some(x) = st.xdg_surface.take() {
        x.destroy();
    }
    if let Some(s) = st.surface.take() {
        s.destroy();
    }
    let _ = conn.flush();
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing(_: &Pending) -> std::io::Result<()> {
        Ok(())
    }

    #[test]
    fn two_samples_give_shares_rates_and_the_windows_first() {
        let m = sample_mon();
        // labwc: 15 ticks in a second on 4 CPUs.
        let labwc = m.rows.iter().find(|r| r.name == "labwc").unwrap();
        assert!((labwc.cpu - 3.75).abs() < 0.01, "{}", labwc.cpu);
        assert_eq!(m.m.cores.len(), 4);
        assert!((m.m.cores[0] - 31.0).abs() < 0.01);
        assert!((m.m.net_down - 1_258_291.0).abs() < 1.0);
        assert!((m.m.disk_read - 6963.0 * 512.0).abs() < 1.0);
        let items = m.items();
        assert_eq!(items[0], Item::Section("APLICACIONES · con ventana"));
        let first = match &items[1] {
            Item::Proc(i) => &m.rows[*i],
            _ => panic!(),
        };
        assert_eq!(first.name, "firefox", "the busiest application first");
        assert!(first.window.as_deref().unwrap().contains("Firefox"));
        assert!(items.contains(&Item::Section("SISTEMA")));
        assert_eq!(m.svcs.len(), 12);
    }

    #[test]
    fn typing_filters_and_esc_clears_then_closes() {
        let mut m = sample_mon();
        for c in "hunt".chars() {
            m.filter.push(c);
        }
        let pids = m.shown_pids();
        assert_eq!(pids, vec![433]);
        assert!(m.key(KEY_ESC_WL, false, 10, &mut nothing));
        assert!(m.filter.is_empty());
        assert!(!m.key(KEY_ESC_WL, false, 10, &mut nothing));
        // A PID is a filter too.
        m.filter = "612".into();
        assert_eq!(m.shown_pids(), vec![612]);
    }

    #[test]
    fn ending_what_the_desktop_stands_on_asks_twice() {
        let mut m = sample_mon();
        let mut done = Vec::new();
        m.sel_pid = Some(612); // labwc
        m.signal_selected(false, &mut |p| {
            done.push(p.clone());
            Ok(())
        });
        assert!(done.is_empty() && m.bad, "{}", m.status);
        m.signal_selected(false, &mut |p| {
            done.push(p.clone());
            Ok(())
        });
        assert_eq!(done, vec![Pending::Signal(612, false)]);
        // Anything else goes at once, and init is never signalled.
        m.sel_pid = Some(1388);
        m.signal_selected(true, &mut |p| {
            done.push(p.clone());
            Ok(())
        });
        assert_eq!(done.last(), Some(&Pending::Signal(1388, true)));
        m.sel_pid = Some(1);
        let n = done.len();
        m.signal_selected(true, &mut |p| {
            done.push(p.clone());
            Ok(())
        });
        assert_eq!(done.len(), n);
    }

    #[test]
    fn services_take_start_stop_restart_and_a_oneshot_none() {
        let mut m = sample_mon();
        m.tab = Tab::Svcs;
        let mut asked = Vec::new();
        m.sel_svc = m.svcs.iter().position(|s| s.name == "ntpd").unwrap();
        m.key(25, false, 10, &mut |p| {
            asked.push(p.clone());
            Ok(())
        }); // P
        assert_eq!(asked, vec![Pending::Service("ntpd".into(), Verb::Stop)]);
        m.sel_svc = 0; // mounts, a oneshot
        m.service_verb(Verb::Restart, &mut |p| {
            asked.push(p.clone());
            Ok(())
        });
        assert_eq!(asked.len(), 1);
        assert!(m.bad);
        // Starting labwc needs no second press; stopping it does.
        m.sel_svc = m.svcs.iter().position(|s| s.name == "labwc").unwrap();
        m.service_verb(Verb::Start, &mut |p| {
            asked.push(p.clone());
            Ok(())
        });
        m.service_verb(Verb::Stop, &mut |p| {
            asked.push(p.clone());
            Ok(())
        });
        assert_eq!(asked.len(), 2);
        // A failure says so.
        m.sel_svc = m.svcs.iter().position(|s| s.name == "ntpd").unwrap();
        m.service_verb(Verb::Restart, &mut |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
        assert!(m.bad && m.status.starts_with("no se pudo"));
    }

    #[test]
    fn a_click_picks_the_tab_the_sort_and_the_row() {
        let (w, h) = (1180, 760);
        let mut m = sample_mon();
        let t = tabs(w);
        m.click(w, h, t[1].0 + 4, t[1].1 + 4, &mut nothing);
        assert_eq!(m.tab, Tab::Svcs);
        m.click(w, h, t[0].0 + 4, t[0].1 + 4, &mut nothing);
        let cy = HEAD_H + STRIP_H + SEARCH_H + 4;
        m.click(w, h, columns(w)[3] - 10, cy, &mut nothing);
        assert_eq!(m.sort, Sort::Mem);
        m.click(w, h, PAD + 4, cy, &mut nothing);
        assert_eq!(m.sort, Sort::Pid);
        let (top, _) = proc_rows(h);
        // Row 0 is the section; row 1 the first application.
        m.click(w, h, 300, top + ROW_H + 4, &mut nothing);
        assert_eq!(m.selected_row().map(|r| r.name.as_str()), Some("foot"), "by PID, foot (1388) is the first app");
    }

    #[test]
    fn arrows_walk_the_list_and_keep_the_selection_on_screen() {
        let mut m = sample_mon();
        m.sel_pid = None;
        m.move_sel(1, 3);
        let first = m.sel_pid;
        assert!(first.is_some());
        for _ in 0..20 {
            m.move_sel(1, 3);
        }
        let pids = m.shown_pids();
        assert_eq!(m.sel_pid, pids.last().copied());
        assert!(m.top > 0, "scrolled to follow it");
    }

    #[test]
    fn every_size_and_tab_draws_without_a_panic() {
        let mut m = sample_mon();
        for tab in [Tab::Procs, Tab::Svcs] {
            m.tab = tab;
            for (w, h) in [(1180, 760), (800, 560), (1920, 1080)] {
                draw(&mut Canvas::new(w as usize, h as usize), w, h, &m);
            }
        }
        let empty = Mon::new();
        draw(&mut Canvas::new(800, 560), 800, 560, &empty);
    }

    #[test]
    fn english_is_the_same_monitor_in_other_words() {
        let mut m = sample_mon();
        m.lang = Lang::En;
        let sections: Vec<_> = m.items().into_iter().filter_map(|i| if let Item::Section(s) = i { Some(s) } else { None }).collect();
        assert_eq!(sections, ["APPLICATIONS · with a window", "SYSTEM"]);
        m.sel_pid = None;
        m.signal_selected(false, &mut |_| Ok(()));
        assert_eq!(m.status, "pick a process from the list");
        for tab in [Tab::Procs, Tab::Svcs] {
            m.tab = tab;
            draw(&mut Canvas::new(1180, 760), 1180, 760, &m);
        }
    }
}
