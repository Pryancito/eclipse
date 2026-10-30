//! lunarhunter — the window for hunter, Eclipse OS's in-kernel security
//! subsystem.
//!
//! The bar's shield menu (`hunter_menu.rs`) is a glance: six modes, three
//! counters, four alerts and four buttons, three of which open a terminal.
//! This is the rest of it, as an application the menu opens:
//!
//! - **Resumen** — the stance in one sentence, every counter, the newest
//!   alerts, the quick actions (scan home, update signatures, lockdown) and the USB sticks and disks mounted now, with their scan.
//! - **Protección** — the six domains with what each one guards, the six
//!   switches (learning, anomaly-detection, privileged-deny, require-hash,
//!   media-scan, media-lock),
//!   saving the live stance as the boot policy, applying `policy.conf`, and
//!   the seal.
//! - **Eventos** — the whole event ring, every field, filtered by severity.
//! - **Escaneo** — an antivirus scan with a live count, that can be stopped,
//!   and quarantines what it finds (each finding, all of them, or as it goes).
//! - **Cuarentena** — what `hunterctl quarantine` put away, and restoring it.
//! - **Firmas** — the signatures the kernel holds, the files they come from,
//!   the feeds, and updating them.
//! - **Programas** — the exec allowlist and the integrity pins: the programs
//!   learning has trusted, saving them to the whitelist, verifying the pins.
//! - **Programados** — the scheduled scans (a quick one daily, a full one
//!   weekly), changing them, running one now, and their history
//!   (`lunarhunter/schedule.rs`).
//! - **Red** — every network socket with the program that holds it, and the
//!   ports that started listening without anyone saying they would, each with
//!   a button that allows it (`net_tab.rs`).
//! - **Cortafuegos** — the per-application firewall: "¿Permitir que X se
//!   conecte a la red?" for every program hunter has no rule for, the rules
//!   already given (and forgetting them), and the firewall's mode. Each answer
//!   goes to the kernel at once and is saved to `/etc/hunter/firewall`.
//!
//! The Resumen tab's "Exportar informe" writes the security report
//! (`hunter_export`) as HTML into `$HOME`; `--export` writes it from the
//! command line with no compositor, which is what `hunterctl report` runs.
//!
//! It holds no policy of its own, exactly like the bar and `hunterctl`: the
//! kernel's doors (`/proc/hunter`, `/proc/hunter_scan`, `/proc/hunter_net`) for everything the
//! kernel does, and `/sbin/hunterctl` for the file work it already does right
//! (the quarantine's moves and index, the feed downloads, the whitelist). So a
//! button here means what the same `hunterctl` command means.
//!
//! An ordinary xdg-shell window, not an overlay like its siblings: a scan of
//! `/` takes minutes, and an overlay that holds the keyboard for that long is
//! a desktop nobody can use. It is in the taskbar, moves and resizes; labwc
//! draws its frame (xdg-decoration, server side).
//!
//! `--export PATH [--days N]` writes the report (HTML, PDF or text by the
//! extension) and exits. `--dump PATH:WxH` renders a tab to a raw ARGB8888 file with no compositor,
//! from this kernel's hunter or, without one, a sample.
//!
//! Env: `ECLIPSE_LOOK=kde|eclipse|win11`, `LANG` (es/en) like the bar.

use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::hunter::{self, MediaState, Mode, SendError, Shield, DOMAINS};
use lunarbar::hunter_export::{self as export, civil_time};
use lunarbar::hunter_media as media;
use lunarbar::hunter_net::{self as net, NetTable};
use lunarbar::hunter_firewall::{self as fw, Dir, Firewall};
use lunarbar::hunter_report::{
    self as report, Feed, Quarantined, Report, Severity, SigFile, Switch,
};
use lunarbar::hunter_scan::{self as scan, Progress};
use lunarbar::i18n::Lang;
use lunarbar::keys::{
    key_char, BTN_LEFT, KEY_DOWN_WL, KEY_ESC_WL, KEY_PGDN_WL, KEY_PGUP_WL, KEY_TAB_WL, KEY_UP_WL,
    WHEEL_NOTCH,
};
use lunarbar::list::inside_rect;
use lunarbar::look::Look;
use lunarbar::proc::{self, free_buffer, map_shm_pool, BUFFERS};
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

/// Where `install_hunter` (xtask) puts the operator's front end.
// A crate root's modules sit beside it, so the path is spelled out.
#[path = "lunarhunter/schedule.rs"]
mod schedule;

use lunarbar::hunter_schedule as sched;
use schedule::SchedAct;

const HUNTERCTL: &str = "/sbin/hunterctl";

/// The *Red* tab. A binary's root resolves `mod` beside itself (`src/bin/`),
/// so the path names the directory of its own.
#[path = "lunarhunter/net_tab.rs"]
mod net_tab;

// ── palette ──────────────────────────────────────────────────────────────────

/// The colours of one look, the same shape `lunarfile` uses so the two read as
/// the same desktop.
struct Palette {
    bg: Rgb,
    rail: Rgb,
    /// Card / row well.
    field: Rgb,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    sel: Rgb,
    sel_text: Rgb,
    on_accent: Rgb,
    border: Rgb,
}

/// The same three in every look: they mean "careful", "bad" and "fine", and
/// have to read as that against all three grounds.
const WARN: Rgb = (0xe0, 0xa8, 0x5e);
const CRIT: Rgb = (0xe8, 0x6a, 0x6a);
const OK: Rgb = (0x6c, 0xc8, 0x8a);
const ON_WARN: Rgb = (0x1a, 0x10, 0x06);

const PAL_KDE: Palette = Palette {
    bg: (0x2a, 0x2e, 0x32),
    rail: (0x23, 0x26, 0x29),
    field: (0x1b, 0x1e, 0x20),
    text: (0xfc, 0xfc, 0xfc),
    dim: (0x9a, 0xa4, 0xa6),
    accent: (0x3d, 0xae, 0xe9),
    sel: (0x3d, 0xae, 0xe9),
    sel_text: (0x0d, 0x14, 0x18),
    on_accent: (0x0d, 0x14, 0x18),
    border: (0x4d, 0x4d, 0x4d),
};

const PAL_ECLIPSE: Palette = Palette {
    bg: (0x0b, 0x12, 0x20),
    rail: (0x08, 0x0e, 0x19),
    field: (0x12, 0x21, 0x38),
    text: (0xff, 0xff, 0xff),
    dim: (0x7d, 0x8f, 0xb3),
    accent: (0x6e, 0xa8, 0xff),
    sel: (0x1f, 0x3a, 0x63),
    sel_text: (0xff, 0xff, 0xff),
    on_accent: (0x0b, 0x12, 0x20),
    border: (0x27, 0x5a, 0x9e),
};

const PAL_WIN11: Palette = Palette {
    bg: (0x20, 0x20, 0x20),
    rail: (0x1a, 0x1a, 0x1a),
    field: (0x2b, 0x2b, 0x2b),
    text: (0xff, 0xff, 0xff),
    dim: (0xc5, 0xc5, 0xc5),
    accent: (0x4c, 0xc2, 0xff),
    sel: (0x00, 0x78, 0xd4),
    sel_text: (0xff, 0xff, 0xff),
    on_accent: (0x00, 0x1a, 0x2e),
    border: (0x3d, 0x3d, 0x3d),
};

fn palette(look: Look) -> &'static Palette {
    match look {
        Look::Win11 => &PAL_WIN11,
        Look::Kde => &PAL_KDE,
        Look::Eclipse => &PAL_ECLIPSE,
    }
}

// ── geometry ─────────────────────────────────────────────────────────────────

const DEFAULT_W: u32 = 960;
const DEFAULT_H: u32 = 640;
const MIN_W: i32 = 760;
const MIN_H: i32 = 500;
const PAD: i32 = 16;
const HEAD_H: i32 = 56;
const RAIL_W: i32 = 180;
const TAB_H: i32 = 36;
const FOOT_H: i32 = 30;
const ROW_H: i32 = 24;
const BTN_H: i32 = 30;
const SEG_W: i32 = 84;
/// Lines of a job's output a tab shows.
const CONSOLE_LINES: usize = 7;
/// Lines of a job's output kept at all.
const MAX_OUTPUT: usize = 400;

type Rect = (i32, i32, i32, i32);

// ── what can be clicked ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Overview,
    Protection,
    Events,
    Scan,
    Quarantine,
    Signatures,
    Programs,
    Schedule,
    Network,
    Firewall,
}

impl Tab {
    const ALL: [Tab; 10] = [
        Tab::Overview,
        Tab::Protection,
        Tab::Events,
        Tab::Scan,
        Tab::Quarantine,
        Tab::Signatures,
        Tab::Programs,
        Tab::Schedule,
        Tab::Network,
        Tab::Firewall,
    ];

    fn label(self, lang: Lang) -> &'static str {
        let (es, en) = match self {
            Tab::Overview => ("Resumen", "Overview"),
            Tab::Protection => ("Protección", "Protection"),
            Tab::Events => ("Eventos", "Events"),
            Tab::Scan => ("Escaneo", "Scan"),
            Tab::Quarantine => ("Cuarentena", "Quarantine"),
            Tab::Signatures => ("Firmas", "Signatures"),
            Tab::Programs => ("Programas", "Programs"),
            Tab::Schedule => ("Programados", "Scheduled"),
            Tab::Network => ("Red", "Network"),
            Tab::Firewall => ("Cortafuegos", "Firewall"),
        };
        tr(lang, es, en)
    }

    /// `--tab NAME`: either language's word, without accents, or the index.
    fn parse(s: &str) -> Option<Tab> {
        let s = s.to_ascii_lowercase();
        Some(match s.as_str() {
            "resumen" | "overview" | "1" => Tab::Overview,
            "proteccion" | "protección" | "protection" | "2" => Tab::Protection,
            "eventos" | "events" | "log" | "registro" | "3" => Tab::Events,
            "escaneo" | "scan" | "4" => Tab::Scan,
            "cuarentena" | "quarantine" | "5" => Tab::Quarantine,
            "firmas" | "signatures" | "6" => Tab::Signatures,
            "programas" | "programs" | "7" => Tab::Programs,
            "programados" | "scheduled" | "schedule" | "8" => Tab::Schedule,
            "red" | "network" | "net" | "9" => Tab::Network,
            "cortafuegos" | "firewall" | "10" => Tab::Firewall,
            _ => return None,
        })
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }
}

/// Which events the Eventos tab lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    /// Warnings and criticals.
    Alerts,
    Critical,
}

impl Filter {
    fn keeps(self, s: Severity) -> bool {
        match self {
            Filter::All => true,
            Filter::Alerts => s >= Severity::Warn,
            Filter::Critical => s == Severity::Crit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Tab(Tab),
    Mode(usize, Mode),
    Switch(Switch, bool),
    Lockdown,
    Seal,
    SavePolicy,
    ApplyPolicy,
    Filter(Filter),
    Target(usize),
    AutoQuarantine(bool),
    StartScan,
    Cancel,
    QuarantineFound(usize),
    QuarantineAll,
    Restore(usize),
    Update,
    SaveLearned,
    Verify,
    QuickScan,
    Export,
    Sched(SchedAct),
    /// Scan removable device `id` again (`hunterctl scan-media`).
    ScanMedia(u64),
    /// Make the `n`th row of the *Red* list an expected listener.
    ExpectPort(usize),
    /// Take back the `n`th expected-port rule.
    ForgetRule(usize),
    NetAlerts(bool),
    /// The firewall's mode.
    NetMode(Mode),
    /// Answer question `i`: allow (true) or deny.
    NetAnswer(usize, bool),
    /// Drop question `i` without deciding.
    NetDismiss(usize),
    /// Drop rule `i`'s program's rules: it is asked about again.
    NetForget(usize),
}

impl Act {
    /// The ones a second click has to confirm: one-way, or they move files.
    fn needs_confirm(self) -> bool {
        matches!(
            self,
            Act::Lockdown
                | Act::Seal
                | Act::SavePolicy
                | Act::Restore(_)
                | Act::QuarantineAll
                | Act::ExpectPort(_)
                | Act::NetAlerts(false)
        )
    }
}

// ── jobs ─────────────────────────────────────────────────────────────────────

/// Work that runs in a thread so the window keeps drawing: a scan, or a
/// `hunterctl` command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Scan,
    Update,
    Verify,
    SaveLearned,
    Quarantine,
    Restore,
    /// `hunterctl schedule` or `run-scheduled`.
    Schedule,
    Media,
    /// `hunterctl expect-port` / `forget-port`.
    Net,
}

impl JobKind {
    /// The tab whose console shows this job's output.
    fn home(self) -> Tab {
        match self {
            JobKind::Scan | JobKind::Quarantine | JobKind::Media => Tab::Scan,
            JobKind::Update => Tab::Signatures,
            JobKind::Verify | JobKind::SaveLearned => Tab::Programs,
            JobKind::Restore => Tab::Quarantine,
            JobKind::Schedule => Tab::Schedule,
            JobKind::Net => Tab::Network,
        }
    }
}

/// A job's output, as the thread publishes it.
#[derive(Clone, Debug, Default)]
struct JobOut {
    lines: Vec<String>,
    done: bool,
    ok: bool,
}

impl JobOut {
    fn push(&mut self, line: String) {
        if self.lines.len() >= MAX_OUTPUT {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }
}

struct Job {
    kind: JobKind,
    out: Arc<Mutex<JobOut>>,
    prog: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    /// Its end has been seen and acted on (`State::job_finished`), once.
    handled: bool,
}

impl Job {
    fn out(&self) -> JobOut {
        self.out.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn prog(&self) -> Progress {
        self.prog.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn running(&self) -> bool {
        !self.out.lock().map(|o| o.done).unwrap_or(true)
    }
}

/// `'...'` for `sh -c`: the only quoting that leaves nothing to expand.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run `hunterctl ARGS` with stderr folded into stdout, publishing each line
/// as it comes (a feed download prints one line per feed, minutes apart).
fn run_ctl(args: &[String], out: &Mutex<JobOut>) -> bool {
    use std::io::BufRead;
    let cmd = std::iter::once(HUNTERCTL.to_string())
        .chain(args.iter().map(|a| sh_quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec {cmd} 2>&1"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            lock(out).push(format!("{HUNTERCTL}: {e}"));
            return false;
        }
    };
    if let Some(so) = child.stdout.take() {
        for line in std::io::BufReader::new(so).lines().map_while(Result::ok) {
            lock(out).push(line);
        }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start `kind` in a thread. `roots` is what a scan walks; `args` what a
/// `hunterctl` job runs.
fn start_job(kind: JobKind, roots: Vec<String>, args: Vec<String>, auto_q: bool) -> Job {
    let job = Job {
        kind,
        out: Arc::new(Mutex::new(JobOut::default())),
        prog: Arc::new(Mutex::new(Progress::default())),
        cancel: Arc::new(AtomicBool::new(false)),
        handled: false,
    };
    let (out, prog, cancel) = (job.out.clone(), job.prog.clone(), job.cancel.clone());
    std::thread::spawn(move || {
        let ok = match kind {
            JobKind::Scan => {
                scan::run(&roots, &prog, &cancel, scan::kernel_batch);
                let p = lock(&prog).clone();
                let mut ok = p.failure.is_none();
                if auto_q && !p.cancelled && !p.found.is_empty() {
                    for f in &p.found {
                        ok &= run_ctl(&["quarantine".into(), f.path.clone()], &out);
                    }
                }
                ok
            }
            _ => run_ctl(&args, &out),
        };
        let mut o = lock(&out);
        o.ok = ok;
        o.done = true;
    });
    job
}

// ── scan targets ─────────────────────────────────────────────────────────────

struct Target {
    label: String,
    roots: Vec<String>,
}

/// The presets the Escaneo tab offers, plus whatever `--scan` named. The
/// keyboard map has no `/` (see `keys::key_char`), so a typed path would be a
/// field nobody can fill; presets, and the command line for anything else.
fn targets(lang: Lang, home: &str, extra: &[String]) -> Vec<Target> {
    let mut t = vec![
        Target {
            label: tr(lang, "Inicio", "Home").into(),
            roots: vec![home.to_string()],
        },
        Target {
            label: "/tmp".into(),
            roots: vec!["/tmp".into(), "/var/tmp".into()],
        },
        Target {
            label: tr(lang, "Programas", "Programs").into(),
            roots: [
                "/bin",
                "/sbin",
                "/usr/bin",
                "/usr/sbin",
                "/usr/local/bin",
                "/opt",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        },
        Target {
            label: tr(lang, "Todo (/)", "All (/)").into(),
            roots: vec!["/".into()],
        },
    ];
    if !extra.is_empty() {
        let label = match extra {
            [one] => one
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(one)
                .to_string(),
            _ => format!("{} {}", extra.len(), tr(lang, "rutas", "paths")),
        };
        t.push(Target {
            label,
            roots: extra.to_vec(),
        });
    }
    t
}

// ── state ────────────────────────────────────────────────────────────────────

/// What the last frame put where.
#[derive(Clone, Debug, Default)]
struct Frame {
    hits: Vec<(Rect, Act)>,
    /// The scrollable list of the current tab and how far it can scroll.
    list: Rect,
    max_scroll: usize,
}

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
    /// The size the last `xdg_toplevel.configure` asked for (0 = ours).
    pending: (i32, i32),
    width: u32,
    height: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    dirty: bool,
    next: usize,
    generation: u64,
    configured: bool,

    pal: &'static Palette,
    lang: Lang,
    root: bool,
    tab: Tab,
    report: Option<Report>,
    /// `/proc/hunter_net`, read while the *Red* tab is open.
    net: Option<NetTable>,
    quarantine: Vec<Quarantined>,
    sig_files: Vec<SigFile>,
    feeds: Vec<Feed>,
    pins: usize,
    /// `/proc/hunter_firewall`; `None` on a kernel without the firewall.
    firewall: Option<Firewall>,
    targets: Vec<Target>,
    target: usize,
    auto_q: bool,
    filter: Filter,
    scroll: usize,
    scroll_acc: f64,
    /// The click that is waiting for its second one.
    confirm: Option<Act>,
    /// What the last action said, and whether it is bad news.
    note: String,
    note_bad: bool,
    job: Option<Job>,
    /// The last scan's results, kept when a quarantine job replaces the job.
    last_scan: Option<Progress>,
    /// Findings already put away, so their buttons go.
    quarantined: Vec<String>,
    sched: sched::Schedule,

    frame: Frame,
    ptr: (f64, f64),
    hover: Option<Act>,
    done: bool,
}

impl State {
    fn new(look: Look, lang: Lang, tab: Tab, extra: &[String]) -> State {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let targets = targets(lang, &home, extra);
        let target = if extra.is_empty() {
            0
        } else {
            targets.len() - 1
        };
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
            width: 0,
            height: 0,
            map: std::ptr::null_mut(),
            map_len: 0,
            buffers: [None, None],
            busy: [false; BUFFERS],
            dirty: false,
            next: 0,
            generation: 0,
            configured: false,
            pal: palette(look),
            lang,
            root: unsafe { libc::geteuid() } == 0,
            tab,
            report: None,
            net: None,
            quarantine: Vec::new(),
            sig_files: Vec::new(),
            feeds: Vec::new(),
            pins: 0,
            firewall: None,
            targets,
            target,
            auto_q: false,
            filter: Filter::All,
            scroll: 0,
            scroll_acc: 0.0,
            confirm: None,
            note: String::new(),
            note_bad: false,
            job: None,
            last_scan: None,
            quarantined: Vec::new(),
            sched: sched::Schedule::default(),
            frame: Frame::default(),
            ptr: (-1.0, -1.0),
            hover: None,
            done: false,
        };
        st.reload_all();
        st
    }

    fn t(&self, es: &'static str, en: &'static str) -> &'static str {
        tr(self.lang, es, en)
    }

    /// Re-read the report and the firewall (a question has to show up while
    /// its program is still waiting), and the network table when its tab is
    /// open (a read of it is a fresh walk of every process's sockets: not
    /// something to do every two seconds for a tab nobody is looking at).
    /// True when any of them changed.
    fn reload_report(&mut self) -> bool {
        let r = report::read();
        let mut changed = false;
        if r != self.report {
            self.report = r;
            changed = true;
        }
        let f = fw::read();
        if f != self.firewall {
            self.firewall = f;
            changed = true;
        }
        if self.tab == Tab::Network {
            let n = net::read();
            if n != self.net {
                self.net = n;
                changed = true;
            }
        }
        changed
    }

    /// Re-read the report and the files beside it.
    fn reload_all(&mut self) {
        self.reload_report();
        self.quarantine = report::read_quarantine();
        self.sig_files = report::read_signature_files();
        self.feeds = std::fs::read_to_string(report::FEEDS)
            .map(|t| report::parse_feeds(&t))
            .unwrap_or_default();
        self.pins = std::fs::read_to_string(report::HASHES)
            .map(|t| report::count_pins(&t))
            .unwrap_or(0);
        self.reload_schedule();
    }

    fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(Job::running)
    }

    fn say(&mut self, msg: impl Into<String>, bad: bool) {
        self.note = msg.into();
        self.note_bad = bad;
    }

    fn send_error(&self, e: SendError) -> &'static str {
        match e {
            SendError::Denied => self.t("hace falta root", "root required"),
            SendError::Rejected => self.t(
                "hunter rechazó el cambio; no se aplicó nada",
                "hunter rejected the change; nothing was applied",
            ),
            SendError::Unavailable => {
                self.t("/proc/hunter no responde", "/proc/hunter not responding")
            }
        }
    }

    /// Send a script and say what came of it. hunter takes a refused
    /// relaxation without an error (it records it in the ring), so "nothing
    /// changed" is checked on the report itself, as the bar's menu does.
    fn send(&mut self, script: &str, expect_change: bool) {
        let before = self.report.clone();
        match hunter::send(script) {
            Ok(()) => {
                self.reload_report();
                let same = |a: &Option<Report>, b: &Option<Report>| match (a, b) {
                    (Some(a), Some(b)) => report::policy_script(a) == report::policy_script(b),
                    _ => false,
                };
                if expect_change && same(&before, &self.report) {
                    let m = self.t("sellado: no se puede relajar", "sealed: cannot relax");
                    self.say(m, true);
                } else {
                    let m = self.t("aplicado", "applied");
                    self.say(m, false);
                }
            }
            Err(e) => {
                let m = self.send_error(e);
                self.say(m, true);
            }
        }
    }

    fn start(&mut self, kind: JobKind, roots: Vec<String>, args: Vec<String>) {
        if self.busy() {
            return;
        }
        // A scan's results outlive the quarantine job that follows them.
        if let Some(j) = &self.job {
            if j.kind == JobKind::Scan {
                self.last_scan = Some(j.prog());
            }
        }
        if kind == JobKind::Scan {
            self.last_scan = None;
            self.quarantined.clear();
        }
        self.job = Some(start_job(kind, roots, args, self.auto_q));
        self.note.clear();
    }

    /// The results the Escaneo tab shows: the running scan's, or the last.
    fn scan_progress(&self) -> Option<Progress> {
        match &self.job {
            Some(j) if j.kind == JobKind::Scan => Some(j.prog()),
            _ => self.last_scan.clone(),
        }
    }

    /// A job that was running has ended: re-read what it may have changed.
    fn job_finished(&mut self) {
        let Some(j) = &mut self.job else { return };
        j.handled = true;
        let j = &*j;
        let out = j.out();
        let kind = j.kind;
        if matches!(kind, JobKind::Quarantine | JobKind::Scan) {
            // One line per file `hunterctl quarantine` put away (a scan with
            // automatic quarantine runs it too).
            let done = out.lines.iter().filter_map(|l| {
                l.strip_prefix("hunterctl: quarantined ")
                    .and_then(|r| r.rsplit_once(" ("))
                    .map(|(p, _)| p.to_string())
            });
            self.quarantined.extend(done.collect::<Vec<_>>());
        }
        if kind == JobKind::Scan {
            let p = j.prog();
            let msg = if p.cancelled {
                self.t("escaneo cancelado", "scan cancelled").to_string()
            } else if let Some(why) = &p.failure {
                why.clone()
            } else if p.found.is_empty() {
                format!(
                    "{} {} {}",
                    self.t("sin amenazas en", "no threats in"),
                    p.done,
                    self.t("archivos", "files")
                )
            } else {
                format!(
                    "{} {}",
                    p.found.len(),
                    self.t("amenazas encontradas", "threats found")
                )
            };
            self.say(msg, p.failure.is_some() || !p.found.is_empty());
        } else if kind == JobKind::Schedule {
            self.sched_finished(&out);
        } else if kind == JobKind::Media {
            // `hunterctl scan-media` ends on its summary, and exits 1 when it
            // found something: that is a verdict, not a command that failed.
            let last = out.lines.last().cloned().unwrap_or_default();
            let last = last
                .strip_prefix("hunterctl: ")
                .unwrap_or(&last)
                .to_string();
            self.say(last, !out.ok);
        } else if !out.ok {
            let m = self.t(
                "la orden falló (ver la consola)",
                "the command failed (see the console)",
            );
            self.say(m, true);
        }
        self.reload_all();
    }

    /// What a click (or key) does.
    fn act(&mut self, a: Act) {
        if a.needs_confirm() && self.confirm != Some(a) {
            self.confirm = Some(a);
            let m = self.t("pulsa otra vez para confirmar", "click again to confirm");
            self.say(m, true);
            return;
        }
        self.confirm = None;
        match a {
            Act::Tab(t) => {
                if t != self.tab {
                    self.tab = t;
                    self.scroll = 0;
                    self.note.clear();
                    self.reload_all();
                }
            }
            Act::Mode(d, m) => {
                if let Some(s) = hunter::mode_script(d, m) {
                    self.send(&s, true);
                }
            }
            Act::Switch(sw, to) => self.send(&sw.script(to), true),
            Act::Lockdown => self.send(hunter::LOCKDOWN, false),
            Act::Seal => self.send("seal\n", false),
            Act::SavePolicy => self.save_policy(),
            Act::ApplyPolicy => match std::fs::read_to_string(report::POLICY) {
                Ok(text) => self.send(&text, false),
                Err(e) => self.say(format!("{}: {e}", report::POLICY), true),
            },
            Act::Filter(f) => {
                self.filter = f;
                self.scroll = 0;
            }
            Act::Target(i) => self.target = i.min(self.targets.len().saturating_sub(1)),
            Act::AutoQuarantine(on) => self.auto_q = on,
            Act::StartScan => {
                let roots = self.targets[self.target].roots.clone();
                self.scroll = 0;
                self.start(JobKind::Scan, roots, Vec::new());
            }
            Act::QuickScan => {
                self.tab = Tab::Scan;
                self.target = 0;
                self.act(Act::StartScan);
            }
            Act::Cancel => {
                if let Some(j) = &self.job {
                    j.cancel.store(true, Ordering::Relaxed);
                }
            }
            Act::QuarantineFound(i) => {
                if let Some(f) = self.scan_progress().and_then(|p| p.found.get(i).cloned()) {
                    self.start(
                        JobKind::Quarantine,
                        Vec::new(),
                        vec!["quarantine".into(), f.path],
                    );
                }
            }
            Act::QuarantineAll => {
                let paths: Vec<String> = self
                    .scan_progress()
                    .map(|p| p.found.into_iter().map(|f| f.path).collect())
                    .unwrap_or_default();
                let paths: Vec<String> = paths
                    .into_iter()
                    .filter(|p| !self.quarantined.contains(p))
                    .collect();
                if !paths.is_empty() {
                    let args = std::iter::once("quarantine".to_string())
                        .chain(paths)
                        .collect();
                    self.start(JobKind::Quarantine, Vec::new(), args);
                }
            }
            Act::Restore(i) => {
                if let Some(q) = self.quarantine.get(i) {
                    let args = vec!["restore".into(), q.sha256.clone()];
                    self.start(JobKind::Restore, Vec::new(), args);
                }
            }
            Act::Update => {
                self.tab = Tab::Signatures;
                self.start(
                    JobKind::Update,
                    Vec::new(),
                    vec!["update-signatures".into()],
                );
            }
            Act::SaveLearned => self.start(
                JobKind::SaveLearned,
                Vec::new(),
                vec!["save-learned".into()],
            ),
            Act::Verify => self.start(JobKind::Verify, Vec::new(), vec!["verify".into()]),
            Act::Export => self.export(),
            Act::Sched(a) => self.sched_act(a),
            Act::ScanMedia(id) => {
                self.tab = Tab::Scan;
                self.start(
                    JobKind::Media,
                    Vec::new(),
                    vec!["scan-media".into(), id.to_string()],
                );
            }
            Act::ExpectPort(i) => {
                let args = self
                    .net
                    .as_ref()
                    .and_then(|t| net_tab::rows(t).get(i).and_then(|s| s.expect_args()));
                if let Some(args) = args {
                    self.start(JobKind::Net, Vec::new(), args);
                }
            }
            Act::ForgetRule(i) => {
                let args = self
                    .net
                    .as_ref()
                    .and_then(|t| t.rules.get(i))
                    .and_then(|r| net::forget_args(r));
                if let Some(args) = args {
                    self.start(JobKind::Net, Vec::new(), args);
                }
            }
            Act::NetAlerts(on) => {
                let script = if on { "alerts on\n" } else { "alerts off\n" };
                match net::send(script) {
                    Ok(()) => {
                        self.reload_report();
                        let m = self.t("aplicado", "applied");
                        self.say(m, false);
                    }
                    Err(e) => self.say(format!("{}: {e}", net::PROC_NET), true),
                }
            }
            Act::NetMode(m) => self.net_send(&fw::mode_script(m)),
            Act::NetAnswer(i, allow) => {
                if let Some(q) = self.question(i) {
                    self.net_send(&fw::answer_script(&q, allow));
                    // A refused `allow` (sealed) is not an error to the
                    // kernel, so it is checked here: the question is still
                    // open.
                    if allow && self.has_question(&q) {
                        let m = self.t("sellado: no se puede permitir", "sealed: cannot allow");
                        self.say(m, true);
                    }
                }
            }
            Act::NetDismiss(i) => {
                if let Some(q) = self.question(i) {
                    self.net_send(&fw::dismiss_script(&q.program));
                }
            }
            Act::NetForget(i) => {
                let program = self
                    .firewall
                    .as_ref()
                    .and_then(|f| f.rules.get(i))
                    .map(|r| r.program.clone());
                if let Some(p) = program {
                    self.net_send(&fw::forget_script(&p));
                }
            }
        }
    }

    /// Write the week's report into `$HOME` (`/tmp` when there is none to
    /// write to), named by the day so last week's stays. Reading only: it
    /// needs no root, and it re-reads everything so the file is what the
    /// kernel says now and not what the window last drew.
    fn export(&mut self) {
        let Some(inp) = export::gather(export::DEFAULT_DAYS) else {
            let m = self.t("no hay hunter que leer", "there is no hunter to read");
            self.say(m, true);
            return;
        };
        let name = export::file_name(inp.now, export::Format::Html);
        let bytes = export::render(&inp, self.lang, export::Format::Html);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let res = export::write(&format!("{home}/{name}"), &bytes)
            .map(|_| format!("{home}/{name}"))
            .or_else(|_| {
                let tmp = format!("/tmp/{name}");
                export::write(&tmp, &bytes).map(|_| tmp)
            });
        match res {
            Ok(path) => {
                let m = format!(
                    "{} {path}",
                    self.t("informe guardado en", "report saved to")
                );
                self.say(m, false);
            }
            Err(e) => self.say(format!("{name}: {e}"), true),
        }
    }

    fn question(&self, i: usize) -> Option<fw::Question> {
        self.firewall.as_ref()?.questions.get(i).cloned()
    }

    fn has_question(&self, q: &fw::Question) -> bool {
        self.firewall.as_ref().is_some_and(|f| {
            f.questions
                .iter()
                .any(|o| o.program == q.program && o.dir == q.dir)
        })
    }

    /// Send a firewall directive, then write the live rules back to
    /// `/etc/hunter/firewall`, so the answer outlives the boot.
    fn net_send(&mut self, script: &str) {
        if let Err(e) = hunter::send(script) {
            let m = self.send_error(e);
            self.say(m, true);
            return;
        }
        self.reload_report();
        let Some(f) = &self.firewall else { return };
        match fw::save(f, &civil_time(now_secs()), fw::RULES_FILE) {
            Ok(()) => {
                let m = self.t(
                    "aplicado y guardado en /etc/hunter/firewall",
                    "applied and saved to /etc/hunter/firewall",
                );
                self.say(m, false);
            }
            Err(e) => self.say(format!("{}: {e}", fw::RULES_FILE), true),
        }
    }

    /// Write the live stance as `/etc/hunter/policy.conf`, the previous file
    /// kept as `.bak`. Through a temporary file and a rename, so a crash
    /// halfway leaves the old policy and not half a new one (the kernel
    /// rejects a file that does not parse, and boots with the defaults).
    fn save_policy(&mut self) {
        let Some(r) = &self.report else { return };
        let text = report::policy_file(r, &civil_time(now_secs()));
        let tmp = format!("{}.new", report::POLICY);
        let res = std::fs::write(&tmp, text).and_then(|_| {
            if std::path::Path::new(report::POLICY).exists() {
                std::fs::copy(report::POLICY, format!("{}.bak", report::POLICY))?;
            }
            std::fs::rename(&tmp, report::POLICY)
        });
        match res {
            Ok(()) => {
                let m = self.t(
                    "guardado en /etc/hunter/policy.conf: se aplicará al arrancar",
                    "saved to /etc/hunter/policy.conf: applied at boot",
                );
                self.say(m, false);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                self.say(format!("{}: {e}", report::POLICY), true);
            }
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        let max = self.frame.max_scroll as i32;
        self.scroll = (self.scroll as i32 + delta).clamp(0, max.max(0)) as usize;
    }

    fn act_at(&self, x: f64, y: f64) -> Option<Act> {
        self.frame
            .hits
            .iter()
            .rev()
            .find(|(r, _)| inside_rect(*r, x, y))
            .map(|(_, a)| *a)
    }

    // ── wayland ──

    fn map_window(&mut self, qh: &QueueHandle<State>) {
        let (Some(comp), Some(wm)) = (&self.compositor, &self.wm_base) else {
            return;
        };
        let surface = comp.create_surface(qh, ());
        let xs = wm.get_xdg_surface(&surface, qh, ());
        let top = xs.get_toplevel(qh, ());
        top.set_title(String::from("lunarhunter"));
        top.set_app_id(String::from("lunarhunter"));
        top.set_min_size(MIN_W, MIN_H);
        if let Some(mgr) = &self.deco_mgr {
            let d = mgr.get_toplevel_decoration(&top, qh, ());
            d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            self.decoration = Some(d);
        }
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xs);
        self.toplevel = Some(top);
    }

    fn configure(&mut self, qh: &QueueHandle<State>) {
        let Some(shm) = self.shm.clone() else { return };
        let (pw, ph) = self.pending;
        let w = if pw > 0 { pw as u32 } else { DEFAULT_W };
        let h = if ph > 0 { ph as u32 } else { DEFAULT_H };
        if fill_guard::check_buffer(w as usize, h as usize).is_err() {
            eprintln!("lunarhunter: {w}x{h} is not renderable; skipping");
            return;
        }
        if self.configured && self.width == w && self.height == h {
            self.render();
            return;
        }
        let Some(geom) = proc::pool_geometry(w, h) else {
            return;
        };
        let (total, stride, frame_size) = (geom.total, geom.stride, geom.frame_size);
        let Some((map, fd)) = map_shm_pool(total, "lunarhunter") else {
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
        self.busy = [false; BUFFERS];
        self.dirty = false;
        self.next = 0;
        self.configured = true;
        self.render();
    }

    fn buffer_released(&mut self, i: usize, generation: u64) {
        if generation != self.generation || i >= BUFFERS {
            return;
        }
        self.busy[i] = false;
        if self.dirty {
            self.render();
        }
    }

    fn render(&mut self) {
        if !self.configured || self.map.is_null() {
            return;
        }
        let Some(surface) = self.surface.clone() else {
            return;
        };
        let i = free_buffer(self.busy, self.next);
        self.dirty = i.is_none();
        let Some(i) = i else { return };
        let (w, h) = (self.width as usize, self.height as usize);
        let frame_size = w * h * 4;
        let Some(mut cv) = Canvas::try_new(w, h) else {
            return;
        };
        self.frame = draw(&mut cv, w as i32, h as i32, self);
        self.scroll = self.scroll.min(self.frame.max_scroll);
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
}

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
        if let Some(d) = self.decoration.take() {
            d.destroy();
        }
        if let Some(t) = self.toplevel.take() {
            t.destroy();
        }
        if let Some(x) = self.xdg_surface.take() {
            x.destroy();
        }
        if let Some(s) = self.surface.take() {
            s.destroy();
        }
    }
}

// ── small helpers ────────────────────────────────────────────────────────────

fn tr(lang: Lang, es: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Es => es,
        Lang::En => en,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cut `s` to what fits in `w` pixels, marking the cut.
fn fit(s: &str, w: i32) -> String {
    let max = (w / GLYPH_W).max(1) as usize;
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).chain(['~']).collect()
}

/// Cut from the left, for paths: the file name is the part worth seeing.
fn fit_left(s: &str, w: i32) -> String {
    let max = (w / GLYPH_W).max(1) as usize;
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    std::iter::once('~')
        .chain(s.chars().skip(n - max + 1))
        .collect()
}

// ── drawing ──────────────────────────────────────────────────────────────────

/// How a button looks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Normal,
    Primary,
    Danger,
}

/// The painter for one frame: the canvas, the palette and the hitboxes, so the
/// tab functions do not each take five arguments to draw a button.
struct Paint<'a> {
    cv: &'a mut Canvas,
    p: &'static Palette,
    st: &'a State,
    hits: Vec<(Rect, Act)>,
}

impl Paint<'_> {
    fn text(&mut self, s: &str, x: i32, y: i32, c: Rgb) {
        self.cv.text(s, x, y, c);
    }

    fn bold(&mut self, s: &str, x: i32, y: i32, c: Rgb) {
        self.cv.text_bold(s, x, y, c);
    }

    /// A button `w` wide (0 = fit its label). `act` None draws it disabled.
    /// Returns its width.
    fn button(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        label: &str,
        look: Style,
        act: Option<Act>,
    ) -> i32 {
        let w = if w > 0 {
            w
        } else {
            Canvas::text_width(label) + 24
        };
        let armed = act.is_some() && act == self.st.confirm;
        let hovered = act.is_some() && act == self.st.hover;
        let p = self.p;
        let (bg, fg, a) = match (act.is_some(), armed, look) {
            (false, _, _) => (p.field, p.dim, 0.6),
            (true, true, _) => (WARN, ON_WARN, 1.0),
            (true, false, Style::Primary) => {
                (p.accent, p.on_accent, if hovered { 0.85 } else { 1.0 })
            }
            (true, false, Style::Danger) => (CRIT, ON_WARN, if hovered { 0.85 } else { 1.0 }),
            (true, false, Style::Normal) => (if hovered { p.sel } else { p.field }, p.text, 1.0),
        };
        self.cv.round_rect_a(x, y, w, BTN_H, 6, bg, a);
        if look == Style::Normal && act.is_some() && !armed {
            self.cv.round_rect_a(x, y, w, BTN_H, 6, p.border, 0.25);
        }
        let label = if armed {
            self.st.t("¿seguro?", "sure?").to_string()
        } else {
            fit(label, w - 10)
        };
        let tx = x + (w - Canvas::text_width(&label)) / 2;
        self.cv.text(&label, tx, y + (BTN_H - GLYPH_H) / 2, fg);
        if let Some(a) = act {
            self.hits.push(((x, y, w, BTN_H), a));
        }
        w
    }

    /// A segmented control at `x`: `labels`, `on` lit, `enabled` deciding
    /// which of the others take a click.
    #[allow(clippy::too_many_arguments)]
    fn segments(
        &mut self,
        x: i32,
        y: i32,
        labels: &[&str],
        on: usize,
        enabled: impl Fn(usize) -> bool,
        action: impl Fn(usize) -> Act,
    ) {
        let p = self.p;
        let h = ROW_H + 2;
        self.cv
            .round_rect(x, y, SEG_W * labels.len() as i32, h, 6, p.field);
        for (i, label) in labels.iter().enumerate() {
            let sx = x + i as i32 * SEG_W;
            let ok = enabled(i);
            let act = action(i);
            let (fg, bg) = if i == on {
                (p.on_accent, Some(p.accent))
            } else if !ok {
                (p.border, None)
            } else if self.st.hover == Some(act) {
                (p.text, Some(p.border))
            } else {
                (p.dim, None)
            };
            if let Some(bg) = bg {
                self.cv.round_rect(sx + 2, y + 2, SEG_W - 4, h - 4, 5, bg);
            }
            let label = fit(label, SEG_W - 8);
            self.cv.text(
                &label,
                sx + (SEG_W - Canvas::text_width(&label)) / 2,
                y + (h - GLYPH_H) / 2,
                fg,
            );
            if ok && i != on {
                self.hits.push(((sx, y, SEG_W, h), act));
            }
        }
    }

    /// A titled card: a rounded well with a dim caption and a big value.
    fn stat(&mut self, x: i32, y: i32, w: i32, caption: &str, value: &str, c: Rgb) {
        let p = self.p;
        self.cv.round_rect(x, y, w, 58, 8, p.field);
        self.text(&fit(caption, w - 20), x + 10, y + 8, p.dim);
        self.bold(&fit(value, w - 20), x + 10, y + 32, c);
    }

    fn section(&mut self, x: i32, y: i32, w: i32, title: &str) {
        let p = self.p;
        self.bold(title, x, y, p.text);
        self.cv.hline(x, y + GLYPH_H + 5, w, p.border, 0.6);
    }

    /// The last lines of a job's output, in a well `h` tall.
    fn console(&mut self, x: i32, y: i32, w: i32, h: i32, lines: &[String], running: bool) {
        let p = self.p;
        self.cv.round_rect(x, y, w, h, 6, p.rail);
        let fits = ((h - 12) / (GLYPH_H + 3)).max(1) as usize;
        let start = lines
            .len()
            .saturating_sub(fits.min(CONSOLE_LINES.max(fits)));
        let mut ly = y + 6;
        for l in &lines[start..] {
            let c = if l.contains("FOUND") || l.contains("FAILED") || l.contains("NO ") {
                CRIT
            } else {
                p.dim
            };
            self.text(&fit(l, w - 16), x + 8, ly, c);
            ly += GLYPH_H + 3;
        }
        if running {
            self.text("...", x + w - 32, y + h - GLYPH_H - 6, p.accent);
        }
    }
}

/// The headline for a report: a word and a colour.
fn stance(st: &State, r: &Report) -> (String, Rgb) {
    let s = &r.status;
    let off = s.modes.iter().filter(|&&m| m == Mode::Off).count();
    let enforcing = s.modes.iter().filter(|&&m| m == Mode::Enforce).count();
    if off > 0 {
        (
            format!(
                "{} {off} {}",
                st.t("Expuesto:", "Exposed:"),
                st.t("dominio(s) apagado(s)", "domain(s) off")
            ),
            CRIT,
        )
    } else if enforcing == DOMAINS.len() {
        (
            st.t("Protegido: todo bloquea", "Protected: everything enforces")
                .into(),
            OK,
        )
    } else {
        (
            format!(
                "{} {} {}",
                st.t("Vigilando:", "Watching:"),
                DOMAINS.len() - enforcing,
                st.t("dominio(s) solo avisan", "domain(s) only report")
            ),
            WARN,
        )
    }
}

/// Paint a whole frame of a `w` x `h` window.
fn draw(cv: &mut Canvas, w: i32, h: i32, st: &State) -> Frame {
    let p = st.pal;
    cv.fill_rect_a(0, 0, w, h, p.bg, 1.0);
    let mut g = Paint {
        cv,
        p,
        st,
        hits: Vec::new(),
    };

    // ── the rail: the shield, the name, the tabs ──
    g.cv.fill_rect_a(0, 0, RAIL_W, h, p.rail, 1.0);
    let shield = st
        .report
        .as_ref()
        .map(|r| r.status.shield(r.status.criticals));
    let (scol, solid) = match shield {
        Some(Shield::Guarded) => (OK, true),
        Some(Shield::Open) => (CRIT, false),
        Some(Shield::Alert) => (WARN, true),
        None => (p.dim, false),
    };
    g.cv.shield_icon(PAD, 16, 24, scol, solid);
    g.bold("lunarhunter", PAD + 32, 20, p.text);
    let mut ty = HEAD_H + 8;
    for t in Tab::ALL {
        let act = Act::Tab(t);
        let on = st.tab == t;
        if on {
            g.cv.round_rect(8, ty, RAIL_W - 16, TAB_H - 4, 6, p.sel);
            g.cv.round_rect(8, ty + 8, 3, TAB_H - 20, 1, p.accent);
        } else if st.hover == Some(act) {
            g.cv.round_rect_a(8, ty, RAIL_W - 16, TAB_H - 4, 6, p.field, 1.0);
        }
        let mut label = t.label(st.lang).to_string();
        // The tab whose job is running says so.
        if st
            .job
            .as_ref()
            .is_some_and(|j| j.running() && j.kind.home() == t)
        {
            label.push_str(" •");
        }
        let badge = match t {
            Tab::Quarantine if !st.quarantine.is_empty() => Some(st.quarantine.len().to_string()),
            Tab::Network => st
                .net
                .as_ref()
                .map(NetTable::unexpected)
                .filter(|&n| n > 0)
                .map(|n| n.to_string()),
            Tab::Firewall => st
                .firewall
                .as_ref()
                .map(|f| f.questions.len())
                .filter(|&n| n > 0)
                .map(|n| n.to_string()),
            Tab::Events => st
                .report
                .as_ref()
                .map(|r| r.status.criticals)
                .filter(|&c| c > 0)
                .map(|c| c.to_string()),
            _ => None,
        };
        let fg = if on { p.sel_text } else { p.text };
        g.text(&label, 22, ty + (TAB_H - 4 - GLYPH_H) / 2, fg);
        if let Some(b) = badge {
            let bw = Canvas::text_width(&b) + 10;
            g.cv.round_rect(RAIL_W - 16 - bw - 4, ty + 7, bw, TAB_H - 18, 8, CRIT);
            g.text(
                &b,
                RAIL_W - 16 - bw + 1,
                ty + (TAB_H - 4 - GLYPH_H) / 2,
                ON_WARN,
            );
        }
        g.hits.push(((8, ty, RAIL_W - 16, TAB_H - 4), act));
        ty += TAB_H;
    }
    if let Some(r) = &st.report {
        let v = format!("hunter v{}", r.version);
        g.text(&v, PAD, h - FOOT_H - GLYPH_H, p.dim);
    }

    // ── the header: the tab's title and the seal ──
    let cx = RAIL_W + PAD + 4;
    let cw = w - cx - PAD - 4;
    g.bold(st.tab.label(st.lang), cx, 20, p.text);
    let mut hx = w - PAD - 4;
    if st.report.as_ref().is_some_and(|r| r.status.sealed) {
        let s = st.t("sellado", "sealed");
        hx -= Canvas::text_width(s);
        g.text(s, hx, 20, WARN);
        hx -= 20;
        g.cv.lock_icon(hx, 20, 14, WARN);
        hx -= 16;
    }
    if !st.root {
        let s = st.t("solo lectura (no eres root)", "read only (not root)");
        hx -= Canvas::text_width(s);
        g.text(s, hx, 20, p.dim);
    }
    g.cv.hline(RAIL_W, HEAD_H, w - RAIL_W, p.border, 0.6);

    let body = (cx, HEAD_H + PAD, cw, h - HEAD_H - PAD - FOOT_H - 6);
    let (mut list, mut max_scroll) = ((0, 0, 0, 0), 0);
    match &st.report {
        None => {
            g.bold(
                st.t("Este núcleo no tiene hunter", "This kernel has no hunter"),
                body.0,
                body.1,
                CRIT,
            );
            g.text(
                st.t(
                    "No hay /proc/hunter. lunarhunter necesita el núcleo de Eclipse OS.",
                    "No /proc/hunter. lunarhunter needs the Eclipse OS kernel.",
                ),
                body.0,
                body.1 + 28,
                p.dim,
            );
        }
        Some(r) => {
            let (l, m) = match st.tab {
                Tab::Overview => (tab_overview(&mut g, body, r), 0),
                Tab::Protection => (tab_protection(&mut g, body, r), 0),
                Tab::Events => tab_events(&mut g, body, r),
                Tab::Scan => tab_scan(&mut g, body),
                Tab::Quarantine => tab_quarantine(&mut g, body),
                Tab::Signatures => (tab_signatures(&mut g, body, r), 0),
                Tab::Programs => tab_programs(&mut g, body, r),
                Tab::Schedule => schedule::tab_schedule(&mut g, body),
                Tab::Network => net_tab::tab_network(&mut g, body, r.status.sealed),
                Tab::Firewall => tab_firewall(&mut g, body),
            };
            list = l;
            max_scroll = m;
        }
    }

    // ── the footer: what the last action said ──
    let fy = h - FOOT_H;
    g.cv.hline(RAIL_W, fy, w - RAIL_W, p.border, 0.6);
    if !st.note.is_empty() {
        let c = if st.note_bad { WARN } else { OK };
        g.text(&fit(&st.note, cw), cx, fy + (FOOT_H - GLYPH_H) / 2, c);
    }
    Frame {
        hits: g.hits,
        list,
        max_scroll,
    }
}

/// Resumen: the stance, the counters, the newest alerts, the quick actions.
fn tab_overview(g: &mut Paint, (x, y, w, h): Rect, r: &Report) -> Rect {
    let st = g.st;
    let p = g.p;
    let s = &r.status;
    let (headline, hc) = stance(st, r);
    g.bold(&fit(&headline, w), x, y, hc);
    let sub = format!(
        "{} {} · {} {} · {} {}",
        st.t("aprendizaje", "learning"),
        if s.learning { "on" } else { "off" },
        st.t("lista de ejecución", "exec allowlist"),
        if r.allowlist_active {
            st.t("activa", "active")
        } else {
            st.t("inactiva", "inactive")
        },
        st.t("sello", "seal"),
        if s.sealed { "on" } else { "off" },
    );
    g.text(&fit(&sub, w), x, y + 22, p.dim);

    // Two rows of four cards.
    let gap = 10;
    let cw = (w - 3 * gap) / 4;
    let crit = if s.criticals > 0 { CRIT } else { p.text };
    let cards: [(&str, String, Rgb); 8] = [
        (st.t("eventos", "events"), r.total.to_string(), p.text),
        (st.t("bloqueos", "blocked"), s.blocked.to_string(), p.text),
        (st.t("avisos", "warnings"), s.warnings.to_string(), p.text),
        (st.t("críticos", "critical"), s.criticals.to_string(), crit),
        (
            st.t("firmas", "signatures"),
            s.signatures.to_string(),
            p.text,
        ),
        (
            st.t("detecciones", "detections"),
            s.detections.to_string(),
            if s.detections > 0 { CRIT } else { p.text },
        ),
        (
            st.t("programas fijados", "pinned programs"),
            s.pinned.to_string(),
            p.text,
        ),
        (
            st.t("en cuarentena", "quarantined"),
            st.quarantine.len().to_string(),
            p.text,
        ),
    ];
    let mut cy = y + 50;
    for (i, (cap, val, c)) in cards.iter().enumerate() {
        let col = (i % 4) as i32;
        if i == 4 {
            cy += 58 + gap;
        }
        g.stat(x + col * (cw + gap), cy, cw, cap, val, *c);
    }
    cy += 58 + 22;

    // Quick actions.
    g.section(x, cy, w, st.t("Acciones rápidas", "Quick actions"));
    cy += 30;
    let busy = st.busy();
    let free = |a: Act| (!busy && st.root).then_some(a);
    let mut bx = x;
    bx += g.button(
        bx,
        cy,
        0,
        st.t("Escanear inicio", "Scan home"),
        Style::Primary,
        free(Act::QuickScan),
    ) + 8;
    bx += g.button(
        bx,
        cy,
        0,
        st.t("Actualizar firmas", "Update signatures"),
        Style::Normal,
        free(Act::Update),
    ) + 8;
    let lock = (st.root
        && !(s.sealed
            && s.modes[2] == Mode::Enforce
            && s.modes[4] == Mode::Enforce
            && s.modes[5] == Mode::Enforce))
        .then_some(Act::Lockdown);
    bx += g.button(bx, cy, 0, st.t("Blindar", "Lockdown"), Style::Danger, lock) + 8;
    // Reading only, so it needs no root; and not while a job runs, so the
    // report is not taken halfway through a scan's quarantine. On a row of
    // its own when the window is too narrow for four buttons.
    let label = st.t("Exportar informe", "Export report");
    if bx + Canvas::text_width(label) + 24 > x + w {
        bx = x;
        cy += BTN_H + 8;
    }
    g.button(
        bx,
        cy,
        0,
        label,
        Style::Normal,
        (!busy).then_some(Act::Export),
    );
    cy += BTN_H + 24;

    if s.media_known {
        cy = media_section(g, (x, cy, w), r, &free);
    }

    // The newest alerts.
    g.section(x, cy, w, st.t("Últimas alertas", "Latest alerts"));
    cy += 30;
    let alerts: Vec<_> = r
        .events
        .iter()
        .rev()
        .filter(|e| e.sev >= Severity::Warn)
        .collect();
    if alerts.is_empty() {
        g.text(st.t("sin alertas", "no alerts"), x, cy, p.dim);
    }
    let room = ((y + h - cy) / ROW_H).max(0) as usize;
    for e in alerts.into_iter().take(room) {
        event_row(g, x, cy, w, e);
        cy += ROW_H;
    }
    (0, 0, 0, 0)
}

/// The USB sticks and disks mounted now: where, from what, the scan's
/// verdict, and a button to scan one again. An infected one lists what was
/// found under it, from `hunterctl scan-media`'s report. Returns where the
/// next section starts.
fn media_section(
    g: &mut Paint,
    (x, mut cy, w): (i32, i32, i32),
    r: &Report,
    free: &dyn Fn(Act) -> Option<Act>,
) -> i32 {
    let st = g.st;
    let p = g.p;
    g.section(
        x,
        cy,
        w,
        st.t("Dispositivos extraíbles", "Removable devices"),
    );
    cy += 30;
    let devs = &r.status.media;
    if devs.is_empty() {
        let none = if r.status.media_scan {
            st.t(
                "ningún dispositivo montado: se escanean al conectarlos",
                "no device mounted: they are scanned when plugged in",
            )
        } else {
            st.t(
                "ningún dispositivo montado (escanear USB está apagado)",
                "no device mounted (scan USB is off)",
            )
        };
        g.text(&fit(none, w), x, cy, p.dim);
        return cy + ROW_H + 14;
    }
    // The newest three: one stick is the usual case, three is already a lot.
    for d in devs.iter().rev().take(3) {
        let c = match d.state {
            MediaState::Infected | MediaState::Failed => CRIT,
            MediaState::Clean => OK,
            MediaState::Skipped => p.dim,
            MediaState::Pending | MediaState::Scanning => p.accent,
        };
        let label = st.t("Escanear", "Scan");
        let bw = Canvas::text_width(label) + 24;
        let busy = matches!(d.state, MediaState::Pending | MediaState::Scanning);
        g.button(
            x + w - bw,
            cy - 6,
            bw,
            label,
            Style::Normal,
            if busy {
                None
            } else {
                free(Act::ScanMedia(d.id))
            },
        );
        let left = format!("{}  ({})", d.target, d.source);
        g.bold(&fit_left(&left, w / 2), x, cy, p.text);
        let v = media::verdict(st.lang, d);
        g.text(&fit(&v, w / 2 - bw - 16), x + w / 2, cy, c);
        cy += ROW_H + 6;
        if d.state == MediaState::Infected {
            for f in media::findings(d.id).iter().take(3) {
                g.text(&fit_left(f, w - 16), x + 16, cy, CRIT);
                cy += GLYPH_H + 4;
            }
        }
    }
    cy + 14
}

/// One event: time, severity, category, text.
fn event_row(g: &mut Paint, x: i32, y: i32, w: i32, e: &report::RingEvent) {
    let p = g.p;
    let c = match e.sev {
        Severity::Crit => CRIT,
        Severity::Warn => WARN,
        Severity::Notice => p.accent,
        Severity::Info => p.dim,
    };
    let ts = format!("{:>9}s", e.ts);
    g.text(&ts, x, y, p.dim);
    let sx = x + 11 * GLYPH_W;
    g.cv.round_rect(sx, y - 1, 6 * GLYPH_W + 8, GLYPH_H + 2, 4, c);
    g.text(e.sev.as_str(), sx + 4, y, ON_WARN);
    let catx = sx + 7 * GLYPH_W + 8;
    g.bold(&fit(&e.category, 10 * GLYPH_W), catx, y, p.text);
    let tx = catx + 11 * GLYPH_W;
    g.text(&fit(&e.text, x + w - tx), tx, y, p.text);
}

/// What each domain guards, for the Protección tab.
fn domain_help(lang: Lang, d: usize) -> &'static str {
    let (es, en) = match d {
        0 => (
            "llamadas al sistema por proceso",
            "per-process system calls",
        ),
        1 => (
            "memoria escribible y ejecutable (W^X)",
            "writable + executable memory (W^X)",
        ),
        2 => ("qué programas pueden ejecutarse", "which programs may run"),
        3 => (
            "heurísticas: floods, fork-bombs",
            "heuristics: floods, fork bombs",
        ),
        4 => (
            "SHA-256 de los programas fijados",
            "SHA-256 of pinned programs",
        ),
        _ => (
            "antivirus por firmas al ejecutar",
            "signature antivirus at exec",
        ),
    };
    tr(lang, es, en)
}

fn switch_text(lang: Lang, sw: Switch) -> (&'static str, &'static str) {
    let (es, en, hes, hen) = match sw {
        Switch::Learning => (
            "aprendizaje",
            "learning",
            "confiar en lo nuevo al primer uso",
            "trust new programs on first use",
        ),
        Switch::AnomalyDetection => (
            "detección de anomalías",
            "anomaly detection",
            "contadores de floods y fork-bombs",
            "flood and fork-bomb counters",
        ),
        Switch::PrivilegedDeny => (
            "denegar privilegiadas",
            "privileged deny",
            "módulos, bpf, kexec, ptrace",
            "modules, bpf, kexec, ptrace",
        ),
        Switch::RequireHash => (
            "exigir hash",
            "require hash",
            "un programa sin fijar es una violación",
            "an unpinned program is a violation",
        ),
        Switch::MediaScan => (
            "escanear USB",
            "scan USB",
            "escanear cada pendrive al montarlo",
            "scan every stick or disk when mounted",
        ),
        Switch::MediaLock => (
            "bloquear USB",
            "lock USB",
            "nada se ejecuta del USB sin escanear",
            "nothing runs from USB until scanned",
        ),
    };
    (tr(lang, es, en), tr(lang, hes, hen))
}

/// Protección: the modes, the switches, the policy and the seal.
fn tab_protection(g: &mut Paint, (x, y, w, _h): Rect, r: &Report) -> Rect {
    let st = g.st;
    let p = g.p;
    let s = &r.status;
    let modes = hunter::text(st.lang).modes;
    let seg_x = x + w - 3 * SEG_W;
    let mut cy = y;
    g.section(x, cy, w, st.t("Dominios", "Domains"));
    cy += 30;
    for (d, name) in DOMAINS.iter().enumerate() {
        g.bold(name, x, cy + 5, p.text);
        g.text(
            &fit(domain_help(st.lang, d), seg_x - x - 120),
            x + 110,
            cy + 5,
            p.dim,
        );
        let on = Mode::ALL.iter().position(|&m| m == s.modes[d]).unwrap_or(0);
        g.segments(
            seg_x,
            cy,
            &modes,
            on,
            |i| st.root && !report::sealed_refuses_mode(r, d, Mode::ALL[i]),
            |i| Act::Mode(d, Mode::ALL[i]),
        );
        cy += ROW_H + 10;
    }
    cy += 10;
    g.section(x, cy, w, st.t("Interruptores", "Switches"));
    cy += 30;
    let seg2 = x + w - 2 * SEG_W;
    for sw in Switch::ALL {
        let (name, help) = switch_text(st.lang, sw);
        g.bold(name, x, cy + 5, p.text);
        g.text(&fit(help, seg2 - x - 230), x + 220, cy + 5, p.dim);
        g.segments(
            seg2,
            cy,
            &["off", "on"],
            sw.get(r) as usize,
            |i| st.root && !report::sealed_refuses_switch(r, sw, i == 1),
            |i| Act::Switch(sw, i == 1),
        );
        // Tighter than the domains: six of them have to leave the buttons
        // and the seal's note inside a window at its default height.
        cy += ROW_H + 5;
    }
    cy += 14;
    let ok = |a: Act| st.root.then_some(a);
    let mut bx = x;
    bx += g.button(
        bx,
        cy,
        0,
        st.t("Guardar como política de arranque", "Save as boot policy"),
        Style::Primary,
        ok(Act::SavePolicy),
    ) + 8;
    bx += g.button(
        bx,
        cy,
        0,
        st.t("Aplicar policy.conf", "Apply policy.conf"),
        Style::Normal,
        ok(Act::ApplyPolicy),
    ) + 8;
    let seal = (st.root && !s.sealed).then_some(Act::Seal);
    g.button(bx, cy, 0, st.t("Sellar", "Seal"), Style::Danger, seal);
    cy += BTN_H + 8;
    g.text(
        &fit(
            st.t(
                "Sellar es de un solo sentido: hasta reiniciar solo se puede endurecer.",
                "Sealing is one way: until reboot, settings can only tighten.",
            ),
            w,
        ),
        x,
        cy,
        p.dim,
    );
    (0, 0, 0, 0)
}

/// Eventos: the whole ring, newest first, filtered.
fn tab_events(g: &mut Paint, (x, y, w, h): Rect, r: &Report) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let mut bx = x;
    for (f, label) in [
        (Filter::All, st.t("Todos", "All")),
        (Filter::Alerts, st.t("Alertas", "Alerts")),
        (Filter::Critical, st.t("Críticos", "Critical")),
    ] {
        let look = if st.filter == f {
            Style::Primary
        } else {
            Style::Normal
        };
        bx += g.button(bx, y, 0, label, look, Some(Act::Filter(f))) + 6;
    }
    let events: Vec<_> = r
        .events
        .iter()
        .rev()
        .filter(|e| st.filter.keeps(e.sev))
        .collect();
    let dropped = if r.dropped + r.critical_dropped > 0 {
        format!(
            " · {} {}",
            r.dropped + r.critical_dropped,
            st.t("descartados", "dropped")
        )
    } else {
        String::new()
    };
    let count = format!(
        "{} / {} {}{dropped}",
        events.len(),
        r.events.len(),
        st.t("en el anillo", "in the ring")
    );
    g.text(&count, bx + 10, y + (BTN_H - GLYPH_H) / 2, p.dim);

    let ly = y + BTN_H + 14;
    let lh = y + h - ly;
    g.cv.round_rect(x - 6, ly - 6, w + 12, lh + 6, 8, p.field);
    let rows = (lh / ROW_H).max(1) as usize;
    let max = events.len().saturating_sub(rows);
    let top = st.scroll.min(max);
    if events.is_empty() {
        g.text(st.t("sin eventos", "no events"), x, ly, p.dim);
    }
    for (i, e) in events.iter().skip(top).take(rows).enumerate() {
        event_row(g, x, ly + i as i32 * ROW_H, w, e);
    }
    ((x, ly, w, lh), max)
}

/// Escaneo: the targets, the run, the findings.
fn tab_scan(g: &mut Paint, (x, y, w, h): Rect) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let busy = st.busy();
    let mut cy = y;
    g.text(
        st.t("Qué escanear:", "What to scan:"),
        x,
        cy + (BTN_H - GLYPH_H) / 2,
        p.dim,
    );
    let mut bx = x + 15 * GLYPH_W;
    for (i, t) in st.targets.iter().enumerate() {
        let look = if st.target == i {
            Style::Primary
        } else {
            Style::Normal
        };
        bx += g.button(bx, cy, 0, &t.label, look, (!busy).then_some(Act::Target(i))) + 6;
    }
    cy += BTN_H + 8;
    let roots = st.targets[st.target].roots.join("  ");
    g.text(&fit(&roots, w), x, cy, p.dim);
    cy += GLYPH_H + 14;

    g.text(
        st.t("Cuarentena automática", "Automatic quarantine"),
        x,
        cy + 6,
        p.text,
    );
    g.segments(
        x + 24 * GLYPH_W,
        cy,
        &["off", "on"],
        st.auto_q as usize,
        |_| !busy,
        |i| Act::AutoQuarantine(i == 1),
    );
    let bxx = x + w;
    if busy && st.job.as_ref().is_some_and(|j| j.kind == JobKind::Scan) {
        let bw = Canvas::text_width(st.t("Cancelar", "Cancel")) + 24;
        g.button(
            bxx - bw,
            cy - 2,
            bw,
            st.t("Cancelar", "Cancel"),
            Style::Danger,
            Some(Act::Cancel),
        );
    } else {
        let label = st.t("Escanear", "Scan");
        let bw = Canvas::text_width(label) + 48;
        g.button(
            bxx - bw,
            cy - 2,
            bw,
            label,
            Style::Primary,
            (!busy && st.root).then_some(Act::StartScan),
        );
    }
    cy += BTN_H + 16;

    let prog = st.scan_progress();
    if let Some(pr) = &prog {
        let frac = if pr.total > 0 {
            pr.done as f32 / pr.total as f32
        } else {
            0.0
        };
        g.cv.round_rect(x, cy, w, 10, 5, p.field);
        let fw = ((w as f32) * frac) as i32;
        if fw > 0 {
            g.cv.round_rect(
                x,
                cy,
                fw.max(10),
                10,
                5,
                if pr.found.is_empty() { p.accent } else { CRIT },
            );
        }
        cy += 18;
        let line = if pr.walking {
            st.t("listando archivos...", "listing files...").to_string()
        } else {
            format!(
                "{} / {} {} · {} {} · {} {}",
                pr.done,
                pr.total,
                st.t("archivos", "files"),
                pr.found.len(),
                st.t("amenazas", "threats"),
                pr.errors,
                st.t("ilegibles", "unreadable")
            )
        };
        g.text(
            &line,
            x,
            cy + (BTN_H - GLYPH_H) / 2,
            if pr.found.is_empty() { p.text } else { CRIT },
        );
        let pending = pr
            .found
            .iter()
            .filter(|f| !st.quarantined.contains(&f.path))
            .count();
        // One finding has its own button; this is for when there are many.
        if pending > 1 {
            let all = (!busy && st.root).then_some(Act::QuarantineAll);
            let label = st.t("Cuarentena a todo", "Quarantine all");
            let bw = Canvas::text_width(label) + 24;
            g.button(x + w - bw, cy, bw, label, Style::Danger, all);
        }
        cy += BTN_H + 10;
    }

    // The findings, each with its own quarantine button.
    let console_h = 4 * (GLYPH_H + 3) + 12;
    let show_console = st.job.as_ref().is_some_and(|j| {
        matches!(j.kind, JobKind::Quarantine | JobKind::Media)
            || (j.kind == JobKind::Scan && st.auto_q)
    });
    let lh = y + h - cy - if show_console { console_h + 10 } else { 0 };
    let found = prog.map(|p| p.found).unwrap_or_default();
    let rows = (lh / (ROW_H + 8)).max(1) as usize;
    let max = found.len().saturating_sub(rows);
    let top = st.scroll.min(max);
    for (i, f) in found.iter().enumerate().skip(top).take(rows) {
        let ry = cy + (i - top) as i32 * (ROW_H + 8);
        g.cv.round_rect(x, ry, w, ROW_H + 4, 6, p.field);
        g.bold(&fit(&f.name, 26 * GLYPH_W), x + 8, ry + 5, CRIT);
        let done = st.quarantined.contains(&f.path);
        let bw = 16 * GLYPH_W;
        g.text(
            &fit_left(&f.path, w - 28 * GLYPH_W - bw - 24),
            x + 28 * GLYPH_W,
            ry + 5,
            p.text,
        );
        if done {
            g.text(st.t("en cuarentena", "quarantined"), x + w - bw, ry + 5, OK);
        } else {
            let act = (!busy && st.root).then_some(Act::QuarantineFound(i));
            g.button(
                x + w - bw - 4,
                ry - 1,
                bw,
                st.t("Cuarentena", "Quarantine"),
                Style::Normal,
                act,
            );
        }
    }
    if show_console {
        if let Some(j) = &st.job {
            let out = j.out();
            g.console(x, y + h - console_h, w, console_h, &out.lines, j.running());
        }
    }
    ((x, cy, w, lh), max)
}

/// Cuarentena: the index, each row restorable.
fn tab_quarantine(g: &mut Paint, (x, y, w, h): Rect) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let help = [
        st.t(
            "Los archivos están en /var/lib/hunter/quarantine, con permisos 000.",
            "Files live in /var/lib/hunter/quarantine, mode 000.",
        ),
        st.t(
            "Restaurar no quita la denegación de su ruta hasta reiniciar.",
            "Restoring does not lift the deny on its path until reboot.",
        ),
    ];
    for (i, l) in help.iter().enumerate() {
        g.text(&fit(l, w), x, y + i as i32 * (GLYPH_H + 4), p.dim);
    }
    let console_h = 3 * (GLYPH_H + 3) + 12;
    let show_console = st.job.as_ref().is_some_and(|j| j.kind == JobKind::Restore);
    let ly = y + 2 * (GLYPH_H + 4) + 14;
    let lh = y + h - ly - if show_console { console_h + 10 } else { 0 };
    let rows = (lh / (ROW_H + 8)).max(1) as usize;
    let items: Vec<_> = st.quarantine.iter().enumerate().rev().collect();
    let max = items.len().saturating_sub(rows);
    let top = st.scroll.min(max);
    if items.is_empty() {
        g.text(
            st.t("la cuarentena está vacía", "the quarantine is empty"),
            x,
            ly + 6,
            p.dim,
        );
    }
    let busy = st.busy();
    for (row, (i, q)) in items.iter().skip(top).take(rows).enumerate() {
        let ry = ly + row as i32 * (ROW_H + 8);
        g.cv.round_rect(x, ry, w, ROW_H + 4, 6, p.field);
        g.text(
            &fit(&q.date.replace('T', " "), 19 * GLYPH_W),
            x + 8,
            ry + 5,
            p.dim,
        );
        g.text(&q.sha256[..12], x + 22 * GLYPH_W, ry + 5, p.dim);
        let bw = 14 * GLYPH_W;
        g.text(
            &fit_left(&q.path, w - 36 * GLYPH_W - bw - 16),
            x + 36 * GLYPH_W,
            ry + 5,
            p.text,
        );
        let act = (!busy && st.root).then_some(Act::Restore(*i));
        g.button(
            x + w - bw - 4,
            ry - 1,
            bw,
            st.t("Restaurar", "Restore"),
            Style::Normal,
            act,
        );
    }
    if show_console {
        if let Some(j) = &st.job {
            g.console(
                x,
                y + h - console_h,
                w,
                console_h,
                &j.out().lines,
                j.running(),
            );
        }
    }
    ((x, ly, w, lh), max)
}

/// Firmas: what the kernel holds, the files, the feeds, updating.
fn tab_signatures(g: &mut Paint, (x, y, w, h): Rect, r: &Report) -> Rect {
    let st = g.st;
    let p = g.p;
    let gap = 10;
    let cw = (w - 3 * gap) / 4;
    g.stat(
        x,
        y,
        cw,
        st.t("firmas cargadas", "signatures loaded"),
        &r.status.signatures.to_string(),
        p.text,
    );
    g.stat(
        x + cw + gap,
        y,
        cw,
        st.t("por hash SHA-256", "by SHA-256 hash"),
        &r.sig_hash.to_string(),
        p.text,
    );
    g.stat(
        x + 2 * (cw + gap),
        y,
        cw,
        st.t("patrones de bytes", "byte patterns"),
        &r.sig_bytes.to_string(),
        p.text,
    );
    let dc = if r.status.detections > 0 {
        CRIT
    } else {
        p.text
    };
    g.stat(
        x + 3 * (cw + gap),
        y,
        cw,
        st.t("detecciones", "detections"),
        &r.status.detections.to_string(),
        dc,
    );
    let mut cy = y + 58 + 20;

    let busy = st.busy();
    let free = |a: Act| (!busy && st.root).then_some(a);
    g.button(
        x,
        cy,
        0,
        st.t("Actualizar ahora", "Update now"),
        Style::Primary,
        free(Act::Update),
    );
    cy += BTN_H + 20;

    g.section(
        x,
        cy,
        w,
        st.t(
            "Archivos de firmas (se cargan al arrancar)",
            "Signature files (loaded at boot)",
        ),
    );
    cy += 28;
    if st.sig_files.is_empty() {
        g.text(st.t("ninguno", "none"), x, cy, p.dim);
        cy += ROW_H;
    }
    for f in st.sig_files.iter().take(5) {
        g.text(&fit_left(&f.path, w - 42 * GLYPH_W), x, cy, p.text);
        let n = format!("{:>6} hash {:>4} bytes", f.hashes, f.patterns);
        g.text(&n, x + w - 40 * GLYPH_W, cy, p.dim);
        let when = f.mtime.map(civil_time).unwrap_or_default();
        g.text(&when, x + w - 16 * GLYPH_W, cy, p.dim);
        cy += ROW_H;
    }
    cy += 8;
    g.section(
        x,
        cy,
        w,
        st.t("Fuentes (/etc/hunter/feeds)", "Feeds (/etc/hunter/feeds)"),
    );
    cy += 28;
    if st.feeds.is_empty() {
        g.text(st.t("ninguna", "none"), x, cy, p.dim);
        cy += ROW_H;
    }
    for f in st.feeds.iter().take(3) {
        g.bold(&fit(&f.name, 26 * GLYPH_W), x, cy, p.text);
        g.text(&f.format, x + 27 * GLYPH_W, cy, p.dim);
        g.text(&fit(&f.url, w - 35 * GLYPH_W), x + 35 * GLYPH_W, cy, p.dim);
        cy += ROW_H;
    }
    if let Some(j) = st.job.as_ref().filter(|j| j.kind.home() == Tab::Signatures) {
        let ch = (y + h - cy - 8).max(GLYPH_H + 12);
        g.console(x, cy + 6, w, ch, &j.out().lines, j.running());
    }
    (0, 0, 0, 0)
}

/// Programas: the allowlist, the learned programs, the pins.
fn tab_programs(g: &mut Paint, (x, y, w, h): Rect, r: &Report) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let gap = 10;
    let cw = (w - 3 * gap) / 4;
    g.stat(
        x,
        y,
        cw,
        st.t("de confianza", "trusted"),
        &r.trusted.to_string(),
        p.text,
    );
    g.stat(
        x + cw + gap,
        y,
        cw,
        st.t("aprendidos", "learned"),
        &r.learned.to_string(),
        p.text,
    );
    g.stat(
        x + 2 * (cw + gap),
        y,
        cw,
        st.t("lista negra", "blacklisted"),
        &r.blacklisted.to_string(),
        p.text,
    );
    let pins = format!("{} / {}", r.status.pinned, st.pins);
    g.stat(
        x + 3 * (cw + gap),
        y,
        cw,
        st.t("fijados núcleo/disco", "pinned kernel/disk"),
        &pins,
        p.text,
    );
    let mut cy = y + 58 + 20;
    let busy = st.busy();
    let free = |a: Act| (!busy && st.root).then_some(a);
    let mut bx = x;
    let save = (!r.learned_programs.is_empty())
        .then_some(Act::SaveLearned)
        .and_then(free);
    bx += g.button(
        bx,
        cy,
        0,
        st.t(
            "Guardar aprendidos en la whitelist",
            "Save learned to the whitelist",
        ),
        Style::Primary,
        save,
    ) + 8;
    g.button(
        bx,
        cy,
        0,
        st.t("Verificar integridad", "Verify integrity"),
        Style::Normal,
        free(Act::Verify),
    );
    cy += BTN_H + 20;
    g.section(
        x,
        cy,
        w,
        st.t(
            "Programas aprendidos (confiados al primer uso)",
            "Learned programs (trusted on first use)",
        ),
    );
    cy += 28;
    let console_h = 5 * (GLYPH_H + 3) + 12;
    let show_console = st
        .job
        .as_ref()
        .is_some_and(|j| j.kind.home() == Tab::Programs);
    let lh = y + h - cy - if show_console { console_h + 10 } else { 0 };
    let rows = (lh / ROW_H).max(1) as usize;
    let progs = &r.learned_programs;
    let max = progs.len().saturating_sub(rows);
    let top = st.scroll.min(max);
    if progs.is_empty() {
        g.text(st.t("ninguno", "none"), x, cy, p.dim);
    }
    for (i, prog) in progs.iter().skip(top).take(rows).enumerate() {
        g.text(&fit_left(prog, w), x, cy + i as i32 * ROW_H, p.text);
    }
    if show_console {
        if let Some(j) = &st.job {
            g.console(
                x,
                y + h - console_h,
                w,
                console_h,
                &j.out().lines,
                j.running(),
            );
        }
    }
    ((x, cy, w, lh), max)
}

/// One line on what the firewall's mode does, for the person choosing it.
fn net_mode_help(st: &State, f: &Firewall) -> String {
    match f.mode {
        Mode::Off => st
            .t(
                "Apagado: no se comprueba nada.",
                "Off: nothing is checked.",
            )
            .into(),
        Mode::Report => st
            .t(
                "Aviso: deja pasar y pregunta.",
                "Report: lets it through and asks.",
            )
            .into(),
        Mode::Enforce => format!(
            "{} {} s{}",
            st.t("Bloqueo: espera tu respuesta", "Enforce: waits for you"),
            f.timeout,
            st.t(", si no, bloquea.", ", then blocks."),
        ),
    }
}

/// Cortafuegos: the open questions with their answers, the rules, the mode.
fn tab_firewall(g: &mut Paint, (x, y, w, h): Rect) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let Some(f) = &st.firewall else {
        g.bold(
            st.t(
                "Este núcleo no tiene cortafuegos",
                "This kernel has no firewall",
            ),
            x,
            y,
            CRIT,
        );
        g.text(
            st.t(
                "No hay /proc/hunter_firewall: hace falta un hunter más nuevo.",
                "No /proc/hunter_firewall: a newer hunter is needed.",
            ),
            x,
            y + 28,
            p.dim,
        );
        return ((0, 0, 0, 0), 0);
    };
    let ok = |a: Act| st.root.then_some(a);
    let modes = hunter::text(st.lang).modes;
    let seg_x = x + w - 3 * SEG_W;
    let mut cy = y;
    g.bold(st.t("Modo", "Mode"), x, cy + 5, p.text);
    g.text(&fit(&net_mode_help(st, f), seg_x - x - 70), x + 60, cy + 5, p.dim);
    let on = Mode::ALL.iter().position(|&m| m == f.mode).unwrap_or(0);
    g.segments(seg_x, cy, &modes, on, |_| st.root, |i| Act::NetMode(Mode::ALL[i]));
    cy += ROW_H + 20;

    // ── the questions ──
    let title = format!(
        "{} ({})",
        st.t("Preguntas", "Questions"),
        f.questions.len()
    );
    g.section(x, cy, w, &title);
    cy += 28;
    // The question across the whole card, the details and the answers under
    // it: a question cut short is not one anybody can answer.
    let card_h = ROW_H + BTN_H + 20;
    // At most half of what is left: the rules below must still show.
    let room = ((y + h - cy) / 2 / (card_h + 6)).max(1) as usize;
    if f.questions.is_empty() {
        g.text(
            st.t(
                "Ningún programa espera respuesta.",
                "No program is waiting for an answer.",
            ),
            x,
            cy,
            p.dim,
        );
        cy += ROW_H + 6;
    }
    // Newest first: that is the one someone is waiting on.
    let newest: Vec<usize> = (0..f.questions.len()).rev().collect();
    for &i in newest.iter().take(room) {
        let q = &f.questions[i];
        g.cv.round_rect(x, cy, w, card_h, 8, p.field);
        g.cv.round_rect(x, cy + 8, 3, card_h - 16, 1, WARN);
        let ask = match q.dir {
            Dir::Out => format!(
                "{} {} {}",
                st.t("¿Permitir que", "Allow"),
                q.name(),
                st.t("se conecte a la red?", "to connect to the network?")
            ),
            Dir::In => format!(
                "{} {} {} {}?",
                st.t("¿Permitir que", "Allow"),
                q.name(),
                st.t("escuche en el puerto", "to listen on port"),
                q.port()
            ),
        };
        let mut bx = x + w - 10;
        let deny_w = Canvas::text_width(st.t("Bloquear", "Block")) + 24;
        let allow_w = Canvas::text_width(st.t("Permitir", "Allow")) + 24;
        let skip_w = Canvas::text_width(st.t("Ignorar", "Ignore")) + 24;
        bx -= skip_w;
        let by = cy + ROW_H + 10;
        g.button(
            bx,
            by,
            skip_w,
            st.t("Ignorar", "Ignore"),
            Style::Normal,
            ok(Act::NetDismiss(i)),
        );
        bx -= deny_w + 8;
        g.button(
            bx,
            by,
            deny_w,
            st.t("Bloquear", "Block"),
            Style::Danger,
            ok(Act::NetAnswer(i, false)),
        );
        bx -= allow_w + 8;
        g.button(
            bx,
            by,
            allow_w,
            st.t("Permitir", "Allow"),
            Style::Primary,
            ok(Act::NetAnswer(i, true)),
        );
        let tw = bx - x - 24;
        g.bold(&fit(&ask, w - 24), x + 14, cy + 8, p.text);
        let ago = f.now.saturating_sub(q.last);
        // The program's path goes last: the name is already in the question,
        // so it is the part that can be cut.
        let detail = format!(
            "{} · {} {} · pid {} · {} {} s · {}",
            q.peer,
            q.count,
            st.t("intento(s)", "attempt(s)"),
            q.pid,
            st.t("hace", "ago"),
            ago,
            q.program,
        );
        g.text(&fit(&detail, tw), x + 14, by + (BTN_H - GLYPH_H) / 2, p.dim);
        cy += card_h + 6;
    }
    if f.questions.len() > room {
        let more = format!(
            "{} {} {}",
            st.t("y", "and"),
            f.questions.len() - room,
            st.t("más", "more")
        );
        g.text(&more, x, cy, p.dim);
        cy += ROW_H;
    }
    cy += 10;

    // ── the rules ──
    let title = format!("{} ({})", st.t("Reglas", "Rules"), f.rules.len());
    g.section(x, cy, w, &title);
    cy += 28;
    let row_h = BTN_H + 6;
    let lh = y + h - cy;
    let rows = (lh / row_h).max(1) as usize;
    let max = f.rules.len().saturating_sub(rows);
    let top = st.scroll.min(max);
    if f.rules.is_empty() {
        g.text(
            st.t(
                "Todavía no hay reglas: se crean al responder.",
                "No rules yet: answering a question makes one.",
            ),
            x,
            cy,
            p.dim,
        );
    }
    for (k, r) in f.rules.iter().enumerate().skip(top).take(rows) {
        let ry = cy + (k - top) as i32 * row_h;
        let ty = ry + (BTN_H - GLYPH_H) / 2;
        let (word, c) = if r.allow {
            (st.t("permitido", "allowed"), OK)
        } else {
            (st.t("bloqueado", "blocked"), CRIT)
        };
        g.bold(word, x, ty, c);
        let dir = match r.dir {
            None => st.t("ambas", "both"),
            Some(Dir::Out) => st.t("salida", "out"),
            Some(Dir::In) => st.t("entrada", "in"),
        };
        g.text(dir, x + 110, ty, p.dim);
        let fw_w = Canvas::text_width(st.t("Olvidar", "Forget")) + 24;
        g.text(&fit_left(&r.program, w - 200 - fw_w), x + 190, ty, p.text);
        g.button(
            x + w - fw_w,
            ry,
            fw_w,
            st.t("Olvidar", "Forget"),
            Style::Normal,
            ok(Act::NetForget(k)),
        );
    }
    ((x, cy, w, lh), max)
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
                "xdg_wm_base" => st.wm_base = Some(registry.bind(name, version.min(2), qh, ())),
                "zxdg_decoration_manager_v1" => st.deco_mgr = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if st.seat.is_none() => {
                    st.seat = Some(registry.bind(name, version.min(5), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        // A client that does not answer is "not responding", and labwc offers
        // to kill it.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        st: &mut Self,
        xs: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xs.ack_configure(serial);
            st.configure(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        st: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            // Applied on the xdg_surface.configure that follows, which is the
            // protocol's "the state is complete".
            xdg_toplevel::Event::Configure { width, height, .. } => {
                st.pending = (width.max(0), height.max(0));
                if width > 0 && width < MIN_W {
                    st.pending.0 = MIN_W;
                }
                if height > 0 && height < MIN_H {
                    st.pending.1 = MIN_H;
                }
            }
            xdg_toplevel::Event::Close => st.done = true,
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
            st.buffer_released(i, generation);
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

/// What a key does: tabs by number or Tab, the list by arrows and pages, Esc
/// disarms a confirmation and otherwise closes, `r` re-reads everything.
fn key(st: &mut State, key: u32) {
    let page = (st.frame.list.3 / ROW_H).max(1);
    match key {
        KEY_ESC_WL => {
            if st.confirm.take().is_some() {
                st.note.clear();
            } else {
                st.done = true;
            }
        }
        KEY_TAB_WL => {
            let next = Tab::ALL[(st.tab.index() + 1) % Tab::ALL.len()];
            st.act(Act::Tab(next));
        }
        KEY_UP_WL => st.scroll_by(-1),
        KEY_DOWN_WL => st.scroll_by(1),
        KEY_PGUP_WL => st.scroll_by(-page),
        KEY_PGDN_WL => st.scroll_by(page),
        k => match key_char(k) {
            Some(c @ '0'..='9') => {
                // 0 is the tenth tab, as on a keyboard's number row.
                let i = if c == '0' { 9 } else { (c as u8 - b'1') as usize };
                let t = Tab::ALL[i];
                st.act(Act::Tab(t));
            }
            Some('r') => {
                st.reload_all();
            }
            _ => return,
        },
    }
    st.render();
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
        if let wl_keyboard::Event::Key {
            key: k,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            key(st, k);
        }
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
                let h = st.act_at(surface_x, surface_y);
                if h != st.hover {
                    st.hover = h;
                    st.render();
                }
            }
            wl_pointer::Event::Leave { .. } => {
                st.ptr = (-1.0, -1.0);
                if st.hover.take().is_some() {
                    st.render();
                }
            }
            wl_pointer::Event::Button {
                button: BTN_LEFT,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                let (x, y) = st.ptr;
                match st.act_at(x, y) {
                    Some(a) => st.act(a),
                    // A click on nothing disarms a pending confirmation.
                    None if st.confirm.take().is_some() => st.note.clear(),
                    None => return,
                }
                st.hover = st.act_at(x, y);
                st.render();
            }
            wl_pointer::Event::Axis { value, .. } => {
                st.scroll_acc += value;
                let notches = (st.scroll_acc / WHEEL_NOTCH).trunc();
                if notches != 0.0 {
                    st.scroll_acc -= notches * WHEEL_NOTCH;
                    st.scroll_by(notches as i32);
                    st.render();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgToplevelDecorationV1,
        _: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        // Whatever the compositor decides, the window works: a client-side
        // frame is not drawn, so at worst it is a bare window labwc can still
        // move with Alt+drag.
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore ZxdgDecorationManagerV1);

// ── the command line ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Cli {
    tab: Option<Tab>,
    /// `--scan PATH...`: a target of its own, scanned at once.
    scan: Vec<String>,
    /// `--update`: update the signatures at once.
    update: bool,
    /// `--quarantine`: the scan quarantines what it finds as it goes, which is
    /// what the bar's "scan" button always did.
    quarantine: bool,
    dump: Option<String>,
    /// `--export PATH`: write the report there and exit.
    export: Option<String>,
    /// `--days N`: the period `--export` covers.
    days: Option<u32>,
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tab" => {
                let t = args.next().ok_or("--tab needs a NAME")?;
                cli.tab = Some(Tab::parse(&t).ok_or(format!("unknown tab {t}"))?);
            }
            "--scan" => {
                while let Some(p) = args.next_if(|n| !n.starts_with('-')) {
                    cli.scan.push(p);
                }
                if cli.scan.is_empty() {
                    return Err("--scan needs a PATH".into());
                }
                // A relative path means the caller's directory, which a
                // thread reading it later would not know.
                if let Some(p) = cli.scan.iter().find(|p| !p.starts_with('/')) {
                    return Err(format!("--scan takes absolute paths ({p})"));
                }
            }
            "--update" => cli.update = true,
            "--quarantine" => cli.quarantine = true,
            "--dump" => cli.dump = Some(args.next().ok_or("--dump needs PATH:WxH")?),
            "--export" => cli.export = Some(args.next().ok_or("--export needs a PATH")?),
            "--days" => {
                let n = args.next().ok_or("--days needs a NUMBER")?;
                match n.parse::<u32>() {
                    Ok(d) if (1..=3650).contains(&d) => cli.days = Some(d),
                    _ => return Err(format!("--days takes 1 to 3650 ({n})")),
                }
            }
            "-h" | "--help" => return Err("help".into()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(cli)
}

fn usage() -> ! {
    eprintln!(
        "usage: lunarhunter [--tab NAME] [--quarantine] [--scan PATH...] [--update]\n\
         \x20                  [--dump PATH:WxH] [--export PATH [--days N]]\n\
         \n\
         \x20 --tab NAME     open on a tab: resumen proteccion eventos escaneo\n\
         \x20                cuarentena firmas programas programados red cortafuegos\n\
         \x20                (or 1-9 and 0, or English)\n\
         \x20 --scan PATH..  scan these absolute paths at once (opens Escaneo)\n\
         \x20 --quarantine   automatic quarantine on (for --scan)\n\
         \x20 --update       update the signatures at once (opens Firmas)\n\
         \x20 --dump P:WxH   render the tab to a raw ARGB8888 file and exit\n\
         \x20 --export PATH  write the security report (.html, .pdf or .txt) and exit\n\
         \x20 --days N       the period the report covers (default 7)\n\
         \n\
         keys: 1-9, 0 or Tab switch tabs, arrows/PgUp/PgDn scroll, r re-reads, Esc closes\n\
         env: ECLIPSE_LOOK=kde|eclipse|win11, LANG"
    );
    std::process::exit(2)
}

/// `--dump PATH:WxH`, the size defaulting to the window's.
fn parse_dump_spec(spec: &str) -> (String, usize, usize) {
    let dims = spec.rsplit_once(':').and_then(|(path, dims)| {
        let (ws, hs) = dims.split_once('x')?;
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(ws) || !digits(hs) {
            return None;
        }
        Some((path.to_string(), ws.parse().ok()?, hs.parse().ok()?))
    });
    dims.unwrap_or((spec.to_string(), DEFAULT_W as usize, DEFAULT_H as usize))
}

/// A report with a little of everything, for `--dump` on a machine with no
/// hunter: what the window looks like, in the kernel's own format.
/// A firewall with a question of each kind and a rule of each, for `--dump`.
const FW_SAMPLE: &str = "mode report\ntimeout 30\nnow 640\n\
    rule allow both /usr/bin/curl\n\
    rule deny out /usr/bin/nc\n\
    pending in 1 88 600 600 0.0.0.0:8080 /usr/bin/python3\n\
    pending out 3 142 610 636 93.184.216.34:443 /usr/bin/wget\n";

const SAMPLE: &str = "hunter security subsystem v0.1.0\n\
enforcement: syscall=enforce wx=report exec=enforce anomaly=report integrity=report av=enforce\n\
events: total=214 blocked=6 warnings=11 reported=5 critical=2 dropped=0 critical_dropped=0\n\
active syscall policies: 0\n\
exec allowlist: active (configured=38, learned=3); blacklist=2; learning=on\n\
heuristics: anomaly-detection=on privileged-deny=off\n\
integrity: pinned=41 require-hash=off\n\
av: signatures=5002 (hash=5000 bytes=2) detections=2\n\
media: auto-scan=on lock=off devices=2\n\
media-device: id=4 state=clean files=523 infected=0 fstype=vfat source=/dev/sdb1 target=/media/FOTOS\n\
media-device: id=5 state=scanning files=0 infected=0 fstype=vfat source=/dev/sdc1 target=/media/usb\n\
learned-programs (3):\n  /usr/bin/firefox\n  /usr/local/bin/eclipse-terminal\n  /bin/busybox\n\
\n\
recent events (oldest first):\n\
[   201] +   812.004s pid=1     NOTICE CONTROL   CONFIG: mode av enforce\n\
[   202] +   815.330s pid=612   INFO   EXEC      ALLOWED: /usr/bin/firefox\n\
[   203] +   902.117s pid=733   WARN   EXEC      REPORTED: untrusted exec /tmp/a.out\n\
[   204] +   902.120s pid=733   WARN   WX        REPORTED: W+X mapping in /tmp/a.out\n\
[   205] +   950.882s pid=801   CRIT   AV        AV BLOCKED: /root/Descargas/eicar.com (EICAR-Test-File)\n\
[   206] +   951.002s pid=802   INFO   EXEC      ALLOWED: /bin/busybox\n\
[   207] +  1003.415s pid=845   WARN   ANOMALY   REPORTED: fork flood (412 forks/s)\n\
[   208] +  1010.000s pid=850   CRIT   INTEGRITY BLOCKED: hash mismatch /usr/bin/su\n";

fn main() {
    let cli = match parse_args_from(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            if e != "help" {
                eprintln!("lunarhunter: {e}");
            }
            usage()
        }
    };
    if let Some(path) = &cli.export {
        std::process::exit(export_to(path, cli.days.unwrap_or(export::DEFAULT_DAYS)));
    }
    let tab = cli.tab.unwrap_or(if !cli.scan.is_empty() {
        Tab::Scan
    } else if cli.update {
        Tab::Signatures
    } else {
        Tab::Overview
    });
    let mut st = State::new(Look::current(), Lang::current(), tab, &cli.scan);
    st.auto_q = cli.quarantine;

    if let Some(spec) = cli.dump.clone() {
        let (path, w, h) = parse_dump_spec(&spec);
        let (w, h) = (w.max(MIN_W as usize), h.max(MIN_H as usize));
        if fill_guard::check_buffer(w, h).is_err() {
            eprintln!("lunarhunter: {w}x{h} is too big; nothing written");
            std::process::exit(1);
        }
        if st.report.is_none() {
            sample(&mut st);
        }
        let Some(mut cv) = Canvas::try_new(w, h) else {
            eprintln!("lunarhunter: cannot allocate a {w}x{h} canvas");
            std::process::exit(1);
        };
        let _ = draw(&mut cv, w as i32, h as i32, &st);
        let mut buf = vec![0u8; w * h * 4];
        if !cv.blit_argb(&mut buf) {
            eprintln!("lunarhunter: blit failed");
            std::process::exit(1);
        }
        if let Err(e) = std::fs::write(&path, &buf) {
            eprintln!("lunarhunter: {path}: {e}");
            std::process::exit(1);
        }
        println!("lunarhunter: wrote {path} ({w}x{h} ARGB8888)");
        return;
    }

    // Not `connect_to_env`: the portal and labwc's autostart start us
    // with no WAYLAND_DISPLAY. See `lunarbar::wl`.
    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarhunter: no Wayland compositor found (is the session up?)");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarhunter: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    if st.wm_base.is_none() || st.shm.is_none() || st.compositor.is_none() {
        eprintln!("lunarhunter: compositor lacks xdg-shell/wl_shm");
        std::process::exit(1);
    }
    st.map_window(&qh);
    if !cli.scan.is_empty() {
        st.act(Act::StartScan);
    } else if cli.update {
        st.act(Act::Update);
    }

    // The report is re-read every this often, so a mode changed from the bar
    // or `hunterctl` shows up here; a running job is polled faster, so its
    // count moves.
    let refresh = Duration::from_secs(2);
    let job_tick = Duration::from_millis(150);
    let mut next_refresh = Instant::now() + refresh;
    while !st.done {
        if queue.flush().is_err() {
            break;
        }
        if let Some(guard) = queue.prepare_read() {
            let deadline = if st.busy() {
                (Instant::now() + job_tick).min(next_refresh)
            } else {
                next_refresh
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let timeout = left.as_micros().div_ceil(1000).min(2000) as i32;
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if ready > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        if queue.dispatch_pending(&mut st).is_err() {
            eprintln!("lunarhunter: protocol error");
            break;
        }
        // A job's end is acted on once, however short it was: one that
        // started and failed within a single turn of this loop was never
        // seen running, and still has to say so.
        if st.busy() {
            st.render();
        } else if st.job.as_ref().is_some_and(|j| !j.handled) {
            st.job_finished();
            st.render();
        }
        if Instant::now() >= next_refresh {
            // The scheduler runs scans whether this window is open or not.
            let sched = st.tab == Tab::Schedule && st.reload_schedule();
            if st.reload_report() | sched {
                st.render();
            }
            next_refresh = Instant::now() + refresh;
        }
    }
    // A scan still running stops with the window: its thread reads `cancel`
    // between batches, and the process ending stops it anyway.
    if let Some(j) = &st.job {
        j.cancel.store(true, Ordering::Relaxed);
    }
}

/// `--export`: the report, from this kernel's hunter, into `path` (`-` is
/// stdout). The exit status says whether it was written.
fn export_to(path: &str, days: u32) -> i32 {
    let Some(inp) = export::gather(days) else {
        eprintln!(
            "lunarhunter: {} is not hunter's report; nothing written",
            hunter::PROC
        );
        return 1;
    };
    let fmt = export::Format::from_path(path);
    let bytes = export::render(&inp, Lang::current(), fmt);
    let res = if path == "-" {
        use std::io::Write;
        std::io::stdout().write_all(&bytes)
    } else {
        export::write(path, &bytes)
    };
    match res {
        Ok(()) => {
            if path != "-" {
                println!("{path}");
            }
            0
        }
        Err(e) => {
            eprintln!("lunarhunter: {path}: {e}");
            1
        }
    }
}

/// Fill a state with the sample, and a finished scan, quarantine and update
/// output to go with it, for `--dump` with no hunter.
fn sample(st: &mut State) {
    st.report = report::parse_report(SAMPLE);
    st.firewall = fw::parse(FW_SAMPLE);
    st.root = true;
    let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    st.quarantine = vec![Quarantined {
        date: "2026-10-29T10:12:40".into(),
        sha256: sha.into(),
        mode: "755".into(),
        path: "/root/Descargas/eicar.com".into(),
    }];
    st.sig_files = vec![
        SigFile {
            path: report::SIGNATURES.into(),
            hashes: 1,
            patterns: 1,
            mtime: Some(1_793_000_000),
        },
        SigFile {
            path: format!("{}/MalwareBazaar.Recent", report::SIGNATURES_D),
            hashes: 4999,
            patterns: 0,
            mtime: Some(1_793_700_000),
        },
    ];
    st.feeds = report::parse_feeds(include_str!("../../../hunterctl/feeds"));
    st.pins = 41;
    st.net = net::parse(net_tab::SAMPLE);
    let found = vec![scan::Finding {
        path: "/root/Descargas/factura.pdf.exe".into(),
        name: "MalwareBazaar.Recent".into(),
    }];
    st.last_scan = Some(Progress {
        total: 1840,
        done: 1840,
        found,
        errors: 3,
        finished: true,
        ..Progress::default()
    });
    let out = JobOut {
        lines: vec!["hunterctl: MalwareBazaar.Recent: 4999 signature(s)".into()],
        done: true,
        ok: true,
    };
    st.job = Some(Job {
        kind: JobKind::Update,
        out: Arc::new(Mutex::new(out)),
        prog: Arc::new(Mutex::new(Progress::default())),
        cancel: Arc::new(AtomicBool::new(false)),
        handled: true,
    });
    schedule::sample(st);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<Cli, String> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    fn sample_state(tab: Tab) -> State {
        let mut st = State::new(Look::Eclipse, Lang::Es, tab, &[]);
        sample(&mut st);
        st
    }

    fn frame(st: &State) -> Frame {
        let mut cv = Canvas::new(DEFAULT_W as usize, DEFAULT_H as usize);
        draw(&mut cv, DEFAULT_W as i32, DEFAULT_H as i32, st)
    }

    #[test]
    fn the_command_line_names_tabs_in_both_languages() {
        assert_eq!(
            cli(&["--tab", "firmas"]).unwrap().tab,
            Some(Tab::Signatures)
        );
        assert_eq!(
            cli(&["--tab", "Quarantine"]).unwrap().tab,
            Some(Tab::Quarantine)
        );
        assert_eq!(cli(&["--tab", "3"]).unwrap().tab, Some(Tab::Events));
        assert!(cli(&["--tab", "nada"]).is_err());
        assert!(cli(&["--tab"]).is_err());
    }

    #[test]
    fn export_takes_a_path_and_a_sane_period() {
        let c = cli(&["--export", "/tmp/i.pdf", "--days", "30"]).unwrap();
        assert_eq!(c.export.as_deref(), Some("/tmp/i.pdf"));
        assert_eq!(c.days, Some(30));
        assert!(cli(&["--export"]).is_err());
        assert!(cli(&["--days", "0"]).is_err());
        assert!(cli(&["--days", "semana"]).is_err());
    }

    #[test]
    fn export_is_offered_without_root_but_not_during_a_job() {
        let mut st = sample_state(Tab::Overview);
        st.root = false;
        assert!(frame(&st).hits.iter().any(|(_, a)| *a == Act::Export));
        // Inside the window, at the default size and at the smallest.
        for (w, h) in [(DEFAULT_W, DEFAULT_H), (MIN_W as u32, MIN_H as u32)] {
            let mut cv = Canvas::new(w as usize, h as usize);
            let f = draw(&mut cv, w as i32, h as i32, &st);
            let (r, _) = f.hits.iter().find(|(_, a)| *a == Act::Export).unwrap();
            assert!(r.0 + r.2 <= w as i32, "{w}x{h}: {r:?}");
        }
        st.job.as_mut().unwrap().out.lock().unwrap().done = false;
        assert!(!frame(&st).hits.iter().any(|(_, a)| *a == Act::Export));
    }

    #[test]
    fn scan_takes_absolute_paths_only() {
        assert_eq!(
            cli(&["--scan", "/root", "/tmp", "--update"]).unwrap().scan,
            ["/root", "/tmp"]
        );
        assert!(cli(&["--scan"]).is_err());
        assert!(cli(&["--scan", "Descargas"]).is_err());
        // What the bar's scan button runs.
        let bar = lunarbar::hunter::app_scan_command("/root");
        let words: Vec<String> = bar
            .split_whitespace()
            .skip(1)
            .map(|w| w.trim_matches('\'').to_string())
            .collect();
        let c = parse_args_from(words.into_iter()).unwrap();
        assert!(c.quarantine);
        assert_eq!(c.scan, ["/root"]);
        for cmd in [
            lunarbar::hunter::app_update_command(),
            lunarbar::hunter::app_events_command(),
        ] {
            let words = cmd.split_whitespace().skip(1).map(String::from);
            assert!(parse_args_from(words).is_ok(), "{cmd}");
        }
    }

    #[test]
    fn every_tab_draws_from_the_sample_and_has_its_rail_hit() {
        for t in Tab::ALL {
            let st = sample_state(t);
            let f = frame(&st);
            for t2 in Tab::ALL {
                assert!(
                    f.hits.iter().any(|(_, a)| *a == Act::Tab(t2)),
                    "{t:?} lacks {t2:?}"
                );
            }
        }
    }

    #[test]
    fn the_overview_offers_a_rescan_only_for_a_device_not_being_scanned() {
        let st = sample_state(Tab::Overview);
        let f = frame(&st);
        let offered = |id| f.hits.iter().any(|(_, a)| *a == Act::ScanMedia(id));
        // The sample: #4 scanned clean, #5 being scanned.
        assert!(offered(4));
        assert!(!offered(5));
    }

    #[test]
    fn the_firewall_tab_asks_about_every_question_and_can_forget_every_rule() {
        let st = sample_state(Tab::Firewall);
        let f = frame(&st);
        let has = |a: Act| f.hits.iter().any(|(_, h)| *h == a);
        let n = st.firewall.as_ref().unwrap().questions.len();
        for i in 0..n {
            assert!(has(Act::NetAnswer(i, true)), "allow {i}");
            assert!(has(Act::NetAnswer(i, false)), "deny {i}");
            assert!(has(Act::NetDismiss(i)), "dismiss {i}");
        }
        for k in 0..st.firewall.as_ref().unwrap().rules.len() {
            assert!(has(Act::NetForget(k)), "forget {k}");
        }
        assert!(has(Act::NetMode(Mode::Enforce)));
        assert!(!has(Act::NetMode(Mode::Report)), "the current mode is lit, not a click");
        assert_eq!(cli(&["--tab", "cortafuegos"]).unwrap().tab, Some(Tab::Firewall));
    }

    #[test]
    fn the_firewall_tab_says_so_on_a_kernel_without_one() {
        let mut st = sample_state(Tab::Firewall);
        st.firewall = None;
        let f = frame(&st);
        assert!(!f.hits.iter().any(|(_, a)| matches!(a, Act::NetAnswer(..))));
    }

    #[test]
    fn the_protection_tab_offers_every_mode_but_the_current_one() {
        let st = sample_state(Tab::Protection);
        let f = frame(&st);
        let r = st.report.as_ref().unwrap();
        for d in 0..DOMAINS.len() {
            for m in Mode::ALL {
                let offered = f.hits.iter().any(|(_, a)| *a == Act::Mode(d, m));
                assert_eq!(offered, m != r.status.modes[d], "{d} {m:?}");
            }
        }
    }

    #[test]
    fn under_the_seal_no_relaxation_is_clickable() {
        let mut st = sample_state(Tab::Protection);
        st.report.as_mut().unwrap().status.sealed = true;
        let f = frame(&st);
        // syscall is at enforce: report and off are gone.
        assert!(!f.hits.iter().any(|(_, a)| *a == Act::Mode(0, Mode::Off)));
        // learning is on: off (tighter) is offered.
        assert!(f
            .hits
            .iter()
            .any(|(_, a)| *a == Act::Switch(Switch::Learning, false)));
        // privileged-deny is off: on (tighter) is offered.
        assert!(f
            .hits
            .iter()
            .any(|(_, a)| *a == Act::Switch(Switch::PrivilegedDeny, true)));
        assert!(!f.hits.iter().any(|(_, a)| *a == Act::Seal));
    }

    #[test]
    fn without_root_nothing_that_changes_hunter_is_clickable() {
        let mut st = sample_state(Tab::Protection);
        st.root = false;
        let f = frame(&st);
        assert!(f.hits.iter().all(|(_, a)| matches!(a, Act::Tab(_))));
    }

    #[test]
    fn lockdown_and_restore_take_two_clicks() {
        let mut st = sample_state(Tab::Quarantine);
        st.act(Act::Restore(0));
        assert_eq!(st.confirm, Some(Act::Restore(0)));
        assert!(
            st.job.as_ref().is_some_and(|j| j.kind == JobKind::Update),
            "no job yet"
        );
        // Anything else disarms it.
        st.act(Act::Filter(Filter::All));
        assert_eq!(st.confirm, None);
    }

    #[test]
    fn the_events_filter_narrows_and_scrolls() {
        let mut st = sample_state(Tab::Events);
        st.filter = Filter::Critical;
        let f = frame(&st);
        assert_eq!(f.max_scroll, 0);
        assert!(Filter::Alerts.keeps(Severity::Crit));
        assert!(!Filter::Alerts.keeps(Severity::Notice));
        st.scroll_by(5);
        assert_eq!(st.scroll, 0, "nothing to scroll with two rows");
    }

    #[test]
    fn findings_get_a_quarantine_button_until_quarantined() {
        let mut st = sample_state(Tab::Scan);
        st.job = None;
        assert!(frame(&st)
            .hits
            .iter()
            .any(|(_, a)| *a == Act::QuarantineFound(0)));
        st.quarantined
            .push("/root/Descargas/factura.pdf.exe".into());
        assert!(!frame(&st)
            .hits
            .iter()
            .any(|(_, a)| *a == Act::QuarantineFound(0)));
    }

    #[test]
    fn an_unexpected_listener_can_be_allowed_and_an_expected_one_cannot() {
        let st = sample_state(Tab::Network);
        let f = frame(&st);
        let rows = net_tab::rows(st.net.as_ref().unwrap());
        let allowed: Vec<usize> = f
            .hits
            .iter()
            .filter_map(|(_, a)| match a {
                Act::ExpectPort(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(allowed, [0]);
        assert_eq!(rows[0].program, "/tmp/.x/nc");
        assert_eq!(
            rows[0].expect_args().unwrap(),
            ["expect-port", "tcp", "4444", "/tmp/.x/nc"]
        );
        // Every rule shown has its remove button.
        for i in 0..3 {
            assert!(f.hits.iter().any(|(_, a)| *a == Act::ForgetRule(i)));
        }
        assert!(f.hits.iter().any(|(_, a)| *a == Act::NetAlerts(false)));
    }

    #[test]
    fn sealed_or_not_root_the_network_tab_does_not_relax() {
        let mut st = sample_state(Tab::Network);
        st.report.as_mut().unwrap().status.sealed = true;
        let f = frame(&st);
        assert!(!f
            .hits
            .iter()
            .any(|(_, a)| matches!(a, Act::ExpectPort(_) | Act::NetAlerts(false))));
        // Forgetting a rule tightens: still there.
        assert!(f.hits.iter().any(|(_, a)| *a == Act::ForgetRule(0)));
        st.root = false;
        let f = frame(&st);
        assert!(f.hits.iter().all(|(_, a)| matches!(a, Act::Tab(_))));
    }

    #[test]
    fn allowing_a_port_takes_two_clicks() {
        let mut st = sample_state(Tab::Network);
        st.act(Act::ExpectPort(0));
        assert_eq!(st.confirm, Some(Act::ExpectPort(0)));
        assert!(st.job.as_ref().is_some_and(|j| j.kind == JobKind::Update));
    }

    #[test]
    fn the_network_tab_is_reachable_by_name_and_number() {
        assert_eq!(cli(&["--tab", "red"]).unwrap().tab, Some(Tab::Network));
        assert_eq!(cli(&["--tab", "9"]).unwrap().tab, Some(Tab::Network));
    }

    #[test]
    fn the_quote_for_sh_leaves_nothing_to_expand() {
        assert_eq!(sh_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(sh_quote("it's$(x)"), "'it'\\''s$(x)'");
    }

    #[test]
    fn civil_time_is_the_calendar() {
        assert_eq!(civil_time(0), "1970-01-01 00:00");
        // 2026-10-29 18:18 UTC.
        assert_eq!(civil_time(1_793_297_880), "2026-10-29 18:18");
        assert_eq!(civil_time(951_782_400), "2000-02-29 00:00");
    }

    #[test]
    fn paths_are_cut_from_the_left() {
        let s = fit_left("/very/long/path/to/file.exe", 10 * GLYPH_W);
        assert!(s.ends_with("file.exe") && s.starts_with('~'));
        assert_eq!(s.chars().count(), 10);
    }

    #[test]
    fn every_job_has_a_home_tab_with_a_console() {
        for k in [
            JobKind::Scan,
            JobKind::Update,
            JobKind::Verify,
            JobKind::SaveLearned,
            JobKind::Quarantine,
            JobKind::Restore,
            JobKind::Media,
            JobKind::Net,
        ] {
            assert_ne!(k.home(), Tab::Overview);
        }
    }
}
