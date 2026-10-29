//! Programados: hunter's scheduled scans in lunarhunter.
//!
//! Each job of `/etc/hunter/schedule` as a card (when it runs, what it does
//! with a finding, when it last ran and when it runs next, and running it
//! now), and the history of runs under them. The scheduler is not in here:
//! it is `hunterctl scheduler`, a service that runs whether this window is
//! open or not. A change here is the `hunterctl schedule` command that says
//! the same, and "run now" is `hunterctl run-scheduled`, so a run started
//! from the window lands in the same history as one the clock started.
//!
//! In a file of its own, beside `lunarhunter.rs` and not inside it, because
//! that file is the one every other tab grows in.

use super::*;
use lunarbar::hunter_schedule::{self as sched, Job as SchedJob, When};

/// What a click on this tab does. `usize` is the job's index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchedAct {
    When(usize, When),
    /// Move the time by this many minutes.
    Time(usize, i32),
    Quarantine(usize, bool),
    Run(usize),
}

/// How far the − and + buttons move a time.
const STEP_MIN: i32 = 30;
const CARD_H: i32 = 3 * (BTN_H + 6) + 14;

/// The local timezone's offset from UTC, in seconds: what `hunterctl` reads
/// from `date +%z`, so the times here are the ones it schedules by.
pub(super) fn local_offset(secs: u64) -> i64 {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        0
    } else {
        tm.tm_gmtoff as i64
    }
}

/// `YYYY-MM-DD HH:MM` in local time.
fn local_time(secs: u64, off: i64) -> String {
    civil_time(secs.saturating_add_signed(off))
}

fn job_label(lang: Lang, name: &str) -> String {
    match name {
        "quick" => tr(lang, "Rápido", "Quick").into(),
        "full" => tr(lang, "Completo", "Full").into(),
        _ => name.into(),
    }
}

fn day_label(lang: Lang, d: u8) -> &'static str {
    const ES: [&str; 7] = [
        "lunes", "martes", "miérc.", "jueves", "viernes", "sábado", "domingo",
    ];
    const EN: [&str; 7] = [
        "Monday", "Tuesday", "Wednes.", "Thursday", "Friday", "Saturday", "Sunday",
    ];
    let d = d as usize % 7;
    tr(lang, ES[d], EN[d])
}

/// The words `hunterctl schedule` takes for `job` with one field changed.
fn schedule_args(job: &SchedJob, when: When, at: u32, quarantine: bool) -> Vec<String> {
    vec![
        "schedule".into(),
        job.name.clone(),
        when.word().into(),
        sched::hhmm(at),
        if quarantine { "quarantine" } else { "report" }.into(),
    ]
}

impl State {
    /// Re-read the schedule. True when it changed.
    pub(super) fn reload_schedule(&mut self) -> bool {
        let s = sched::read();
        if s != self.sched {
            self.sched = s;
            true
        } else {
            false
        }
    }

    pub(super) fn sched_act(&mut self, a: SchedAct) {
        let i = match a {
            SchedAct::When(i, _)
            | SchedAct::Time(i, _)
            | SchedAct::Quarantine(i, _)
            | SchedAct::Run(i) => i,
        };
        let Some(job) = self.sched.jobs.get(i).cloned() else {
            return;
        };
        let args = match a {
            SchedAct::When(_, w) => schedule_args(&job, w, job.at, job.quarantine),
            SchedAct::Time(_, d) => {
                let at = (job.at as i32 + d).rem_euclid(24 * 60) as u32;
                schedule_args(&job, job.when, at, job.quarantine)
            }
            SchedAct::Quarantine(_, q) => schedule_args(&job, job.when, job.at, q),
            SchedAct::Run(_) => vec!["run-scheduled".into(), job.name.clone()],
        };
        // The option already lit takes a click like the others; it changes
        // nothing, so it runs nothing.
        if args == schedule_args(&job, job.when, job.at, job.quarantine) {
            return;
        }
        self.start(JobKind::Schedule, Vec::new(), args);
    }

    /// A `hunterctl schedule` or `run-scheduled` has ended. A run says how
    /// it went in its last line; that is the footer's note, and a run that
    /// found something exits 1 without having failed.
    pub(super) fn sched_finished(&mut self, out: &JobOut) {
        let last = out
            .lines
            .iter()
            .rev()
            .find(|l| l.starts_with("hunterctl: "))
            .cloned();
        match last {
            Some(l) if l.contains(" file(s), ") => {
                let bad = !l.contains(": clean,");
                self.say(l.trim_start_matches("hunterctl: ").to_string(), bad);
            }
            Some(l) if !out.ok => self.say(l.trim_start_matches("hunterctl: ").to_string(), true),
            _ if !out.ok => {
                let m = self.t(
                    "la orden falló (ver la consola)",
                    "the command failed (see the console)",
                );
                self.say(m, true);
            }
            _ => {
                let m = self.t("programación guardada", "schedule saved");
                self.say(m, false);
            }
        }
    }
}

/// A run's outcome in a few words, and its colour.
fn run_status(lang: Lang, r: &sched::Run) -> (String, Rgb) {
    match r.status.as_str() {
        "clean" => (tr(lang, "limpio", "clean").to_string(), OK),
        "infected" if r.quarantined > 0 => (
            format!(
                "{} {}, {} {}",
                tr(lang, "amenazas:", "threats:"),
                r.infected,
                tr(lang, "aisladas:", "quarantined:"),
                r.quarantined
            ),
            CRIT,
        ),
        "infected" => (
            format!("{} {}", tr(lang, "amenazas:", "threats:"), r.infected),
            CRIT,
        ),
        _ => (tr(lang, "falló", "failed").to_string(), WARN),
    }
}

/// Programados: a card per job, then the history.
pub(super) fn tab_schedule(g: &mut Paint, (x, y, w, h): Rect) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let lang = st.lang;
    let s = &st.sched;
    let now = now_secs();
    let off = local_offset(now);
    if !s.present {
        g.bold(
            st.t("No hay /etc/hunter/schedule", "No /etc/hunter/schedule"),
            x,
            y,
            CRIT,
        );
        g.text(
            &fit(
                st.t(
                    "Esta imagen es anterior a los escaneos programados.",
                    "This image predates scheduled scans.",
                ),
                w,
            ),
            x,
            y + 28,
            p.dim,
        );
        return ((x, y, w, 0), 0);
    }
    let help = [
        st.t(
            "Los lanza el servicio hunter-scheduler, sin esta ventana.",
            "The hunter-scheduler service runs them, window or not.",
        ),
        st.t(
            "Si el equipo está apagado a esa hora, se hacen al volver.",
            "If the machine is off then, they run when it is back.",
        ),
    ];
    for (k, l) in help.iter().enumerate() {
        g.text(&fit(l, w), x, y + k as i32 * (GLYPH_H + 4), p.dim);
    }
    let busy = st.busy();
    let can = !busy && st.root;
    let mut cy = y + 2 * (GLYPH_H + 4) + 8;
    for (i, j) in s.jobs.iter().enumerate() {
        job_card(g, (x, cy, w), i, j, can, now, off);
        cy += CARD_H + 8;
    }

    // ── the history ──
    cy += 4;
    g.section(x, cy, w, st.t("Historial", "History"));
    if let Some(pid) = s.running {
        let m = format!(
            "{} (pid {pid})",
            st.t("escaneo programado en curso", "scheduled scan running")
        );
        let mw = Canvas::text_width(&m);
        g.text(&m, x + w - mw, cy, WARN);
    }
    cy += GLYPH_H + 14;
    let console_h = 4 * (GLYPH_H + 3) + 12;
    let show_console = st.job.as_ref().is_some_and(|j| j.kind == JobKind::Schedule);
    let lh = y + h - cy - if show_console { console_h + 10 } else { 0 };
    let rows = (lh / ROW_H).max(1) as usize;
    let max = s.history.len().saturating_sub(rows);
    let top = st.scroll.min(max);
    if s.history.is_empty() {
        g.text(
            st.t("todavía no se ha hecho ninguno", "none has run yet"),
            x,
            cy + 4,
            p.dim,
        );
    }
    // Newest first.
    for (row, r) in s.history.iter().rev().skip(top).take(rows).enumerate() {
        let ry = cy + row as i32 * ROW_H;
        let col = |n: i32| x + n * GLYPH_W;
        // Who started it and how long it took only where there is room: a
        // narrow window keeps what matters, the result.
        let wide = w / GLYPH_W >= 80;
        g.text(
            &r.date.replace('T', " ")[..r.date.len().min(16)],
            col(0),
            ry,
            p.dim,
        );
        g.text(
            &fit(&job_label(lang, &r.job), 9 * GLYPH_W),
            col(18),
            ry,
            p.text,
        );
        let mut cx = 28;
        if wide {
            let how = if r.trigger == "manual" {
                tr(lang, "a mano", "manual")
            } else {
                tr(lang, "reloj", "clock")
            };
            g.text(how, col(cx), ry, p.dim);
            cx += 7;
        }
        g.text(
            &format!("{} {}", r.files, tr(lang, "arch.", "files")),
            col(cx),
            ry,
            p.text,
        );
        cx += 13;
        if wide {
            let dur = if r.secs >= 60 {
                format!("{}m{:02}s", r.secs / 60, r.secs % 60)
            } else {
                format!("{}s", r.secs)
            };
            g.text(&dur, col(cx), ry, p.dim);
            cx += 8;
        }
        let (status, c) = run_status(lang, r);
        g.text(&fit(&status, x + w - col(cx)), col(cx), ry, c);
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

#[allow(clippy::too_many_arguments)]
fn job_card(
    g: &mut Paint,
    (x, y, w): (i32, i32, i32),
    i: usize,
    j: &SchedJob,
    can: bool,
    now: u64,
    off: i64,
) {
    let st = g.st;
    let p = g.p;
    let lang = st.lang;
    g.cv.round_rect(x, y, w, CARD_H, 8, p.field);
    let (ix, iw) = (x + 12, w - 24);
    let row = |n: i32| y + 7 + n * (BTN_H + 6);
    let mid = |n: i32| row(n) + (BTN_H - GLYPH_H) / 2;

    // Row 1: the name, the next run, run now.
    let name = job_label(lang, &j.name);
    g.bold(&name, ix, mid(0), p.text);
    let run = st.t("Ejecutar ahora", "Run now");
    let rw = Canvas::text_width(run) + 24;
    let nx = ix + Canvas::text_width(&name) + 16;
    let next = match sched::next_slot(j.last.unwrap_or(now), j.when, j.at, off) {
        None => st.t("desactivado", "disabled").to_string(),
        // Due already: the scheduler looks once a minute.
        Some(n) if n <= now => st.t("próximo: ahora", "next: now").to_string(),
        Some(n) => format!("{} {}", st.t("próximo:", "next:"), local_time(n, off)),
    };
    let nc = if j.when == When::Off { WARN } else { p.text };
    g.text(&fit(&next, ix + iw - rw - 12 - nx), nx, mid(0), nc);
    g.button(
        ix + iw - rw,
        row(0),
        rw,
        run,
        Style::Primary,
        can.then_some(Act::Sched(SchedAct::Run(i))),
    );

    // Row 2: when, the day, the time; the next run.
    let on = match j.when {
        When::Off => 0,
        When::Daily => 1,
        When::Weekly(_) => 2,
    };
    // Back to weekly from elsewhere, the day it had; the shipped one if none.
    let day = match j.when {
        When::Weekly(d) => d,
        _ => 6,
    };
    let labels = [
        st.t("no", "off"),
        st.t("diario", "daily"),
        st.t("semanal", "weekly"),
    ];
    let whens = [When::Off, When::Daily, When::Weekly(day)];
    g.segments(
        ix,
        row(1) + 2,
        &labels,
        on,
        |_| can,
        |k| Act::Sched(SchedAct::When(i, whens[k])),
    );
    let mut bx = ix + 3 * SEG_W + 10;
    if let When::Weekly(d) = j.when {
        let next_day = When::Weekly((d + 1) % 7);
        bx += g.button(
            bx,
            row(1),
            11 * GLYPH_W,
            day_label(lang, d),
            Style::Normal,
            can.then_some(Act::Sched(SchedAct::When(i, next_day))),
        ) + 8;
    }
    if j.when != When::Off {
        let tw = 30;
        g.button(
            bx,
            row(1),
            tw,
            "-",
            Style::Normal,
            can.then_some(Act::Sched(SchedAct::Time(i, -STEP_MIN))),
        );
        let at = j.at_str();
        g.bold(&at, bx + tw + 8, mid(1), p.text);
        let ax = bx + tw + 16 + Canvas::text_width(&at);
        g.button(
            ax,
            row(1),
            tw,
            "+",
            Style::Normal,
            can.then_some(Act::Sched(SchedAct::Time(i, STEP_MIN))),
        );
    }

    // Row 3: what a finding gets; the paths. (The last run is the history's
    // top line, right under the cards.)
    let lead = st.t("Si encuentra algo:", "On a finding:");
    g.text(lead, ix, mid(2), p.dim);
    let mut bx = ix + Canvas::text_width(lead) + 10;
    for (k, label) in [st.t("avisar", "alert"), st.t("cuarentena", "quarantine")]
        .into_iter()
        .enumerate()
    {
        let on = j.quarantine == (k == 1);
        let look = if on { Style::Primary } else { Style::Normal };
        let act = can.then_some(Act::Sched(SchedAct::Quarantine(i, k == 1)));
        bx += g.button(bx, row(2), 0, label, look, act) + 6;
    }
    bx += 10;
    g.text(
        &fit_left(&j.paths.join(" "), ix + iw - bx),
        bx,
        mid(2),
        p.dim,
    );
}

/// A schedule and a history for `--dump` with no hunter.
pub(super) fn sample(st: &mut State) {
    let mut jobs = sched::parse_schedule(include_str!("../../../../hunterctl/schedule"));
    // When the history below says each last started.
    jobs[0].last = Some(1_793_275_202);
    jobs[1].last = Some(1_793_297_560);
    let history = sched::parse_history(
        "2026-10-28T12:00:04\tquick\tscheduled\t38\t1811\t0\t0\tclean\t20261028-120004-quick.log\n\
         2026-10-29T12:00:02\tquick\tscheduled\t41\t1840\t1\t0\tinfected\t20261029-120002-quick.log\n\
         2026-10-29T18:12:40\tfull\tmanual\t611\t90210\t1\t1\tinfected\t20261029-181240-full.log\n",
    );
    st.sched = sched::Schedule {
        jobs,
        history,
        running: None,
        present: true,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(tab: Tab) -> State {
        let mut st = State::new(Look::Eclipse, Lang::Es, tab, &[]);
        super::super::sample(&mut st);
        st
    }

    fn frame(st: &State) -> Frame {
        let mut cv = Canvas::new(DEFAULT_W as usize, DEFAULT_H as usize);
        draw(&mut cv, DEFAULT_W as i32, DEFAULT_H as i32, st)
    }

    #[test]
    fn every_job_has_its_controls_and_a_run_button() {
        let st = sample_state(Tab::Schedule);
        let f = frame(&st);
        let has = |a: SchedAct| f.hits.iter().any(|(_, b)| *b == Act::Sched(a));
        for i in 0..st.sched.jobs.len() {
            assert!(has(SchedAct::Run(i)), "job {i}: run");
            assert!(has(SchedAct::When(i, When::Off)), "job {i}: off");
            assert!(has(SchedAct::Time(i, STEP_MIN)), "job {i}: +");
        }
        // The full scan is weekly on Sunday: its day button moves it to Monday.
        assert!(has(SchedAct::When(1, When::Weekly(0))));
    }

    #[test]
    fn without_root_the_schedule_is_read_only() {
        let mut st = sample_state(Tab::Schedule);
        st.root = false;
        let f = frame(&st);
        assert!(!f.hits.iter().any(|(_, a)| matches!(a, Act::Sched(_))));
    }

    #[test]
    fn a_change_is_the_hunterctl_command_that_says_it() {
        let st = sample_state(Tab::Schedule);
        let quick = &st.sched.jobs[0];
        assert_eq!(
            schedule_args(quick, When::Weekly(0), 11 * 60 + 30, true),
            ["schedule", "quick", "mon", "11:30", "quarantine"]
        );
        // The time wraps round midnight rather than leaving the day.
        let at = (30i32 - STEP_MIN * 2).rem_euclid(24 * 60) as u32;
        assert_eq!(sched::hhmm(at), "23:30");
    }
}
