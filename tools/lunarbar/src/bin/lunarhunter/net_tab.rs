//! Red: hunter's network monitor (`/proc/hunter_net`). Which program holds
//! which port, who it talks to, and the listeners nobody said to expect, each
//! with a button that makes it expected (`hunterctl expect-port`, saved in
//! `/etc/hunter/ports`). Its own file for the size ratchet; it draws with the
//! window's painter like every other tab.

use lunarbar::draw::GLYPH_H;
use lunarbar::draw::GLYPH_W;
use lunarbar::hunter_net::{NetTable, Verdict};

use super::{fit, fit_left, Act, JobKind, Paint, Rect, Style, BTN_H, CRIT, OK, ROW_H, WARN};

/// A table with a little of everything, for `--dump` with no hunter.
pub(super) const SAMPLE: &str = "hunter netmon\n\
alerts=on ignore-loopback=on sockets=6 listeners=4 unexpected=1 reported=1\n\
expect udp 68\n\
expect udp 546\n\
expect tcp 22 /usr/sbin/sshd\n\
sock 412 tcp LISTEN 0.0.0.0:22 - listen /usr/sbin/sshd\n\
sock 733 tcp LISTEN 0.0.0.0:4444 - ALERT /tmp/.x/nc\n\
sock 210 udp UNCONN 0.0.0.0:68 - listen /sbin/edhcpc\n\
sock 515 tcp LISTEN 127.0.0.1:631 - listen /usr/sbin/cupsd\n\
sock 612 tcp ESTAB 10.0.2.15:49822 93.184.216.34:443 ok /usr/bin/firefox\n\
sock 612 tcp ESTAB 10.0.2.15:49830 140.82.112.4:443 ok /usr/bin/firefox\n";

/// The rows the list shows, in the order [`Act::ExpectPort`] counts them.
pub(super) fn rows(t: &NetTable) -> Vec<&lunarbar::hunter_net::NetSock> {
    t.sorted()
}

pub(super) fn tab_network(g: &mut Paint, (x, y, w, h): Rect, sealed: bool) -> (Rect, usize) {
    let st = g.st;
    let p = g.p;
    let Some(t) = &st.net else {
        g.bold(
            st.t(
                "Este núcleo no tiene monitor de red",
                "This kernel has no network monitor",
            ),
            x,
            y,
            CRIT,
        );
        g.text(
            st.t(
                "No hay /proc/hunter_net: hace falta un hunter más nuevo.",
                "No /proc/hunter_net: a newer hunter is needed.",
            ),
            x,
            y + 28,
            p.dim,
        );
        return ((x, y, w, 0), 0);
    };

    // ── the counters ──
    let gap = 10;
    let cw = (w - 3 * gap) / 4;
    let unexpected = t.unexpected();
    let alert_c = if unexpected > 0 { CRIT } else { OK };
    let stats = [
        (
            st.t("en escucha", "listening"),
            t.listeners().to_string(),
            p.text,
        ),
        (
            st.t("inesperados", "unexpected"),
            unexpected.to_string(),
            alert_c,
        ),
        (
            st.t("conexiones", "connections"),
            t.socks
                .iter()
                .filter(|s| s.verdict == Verdict::Connected)
                .count()
                .to_string(),
            p.text,
        ),
        (
            st.t("procesos", "processes"),
            t.processes().to_string(),
            p.text,
        ),
    ];
    for (i, (caption, value, c)) in stats.iter().enumerate() {
        g.stat(x + i as i32 * (cw + gap), y, cw, caption, value, *c);
    }

    // ── the switches ──
    let mut cy = y + 58 + 14;
    let busy = st.busy();
    let free = |a: Act| (!busy && st.root).then_some(a);
    let alerts_act = if t.alerts {
        (!sealed).then_some(Act::NetAlerts(false)).and_then(free)
    } else {
        free(Act::NetAlerts(true))
    };
    let label = if t.alerts {
        st.t("Avisos: activados", "Alerts: on")
    } else {
        st.t("Avisos: desactivados", "Alerts: off")
    };
    let look = if t.alerts {
        Style::Normal
    } else {
        Style::Danger
    };
    let bw = g.button(x, cy, 0, label, look, alerts_act);
    let note = if t.ignore_loopback {
        st.t(
            "Los puertos solo de 127.0.0.1 no avisan: no se ven desde la red.",
            "Ports on 127.0.0.1 only do not alert: the network cannot reach them.",
        )
    } else {
        st.t(
            "También avisan los puertos solo de 127.0.0.1.",
            "Ports on 127.0.0.1 only alert too.",
        )
    };
    g.text(
        &fit(note, w - bw - 16),
        x + bw + 12,
        cy + (BTN_H - GLYPH_H) / 2,
        p.dim,
    );
    cy += BTN_H + 16;

    // ── the sockets ──
    g.section(
        x,
        cy,
        w,
        st.t("Conexiones por proceso", "Connections by process"),
    );
    cy += 26;
    let console_h = 3 * (GLYPH_H + 3) + 12;
    let show_console = st.job.as_ref().is_some_and(|j| j.kind == JobKind::Net);
    let rules_h = (t.rules.len().min(4) as i32) * (ROW_H + 6) + 34;
    let lh = y + h - cy - rules_h - if show_console { console_h + 10 } else { 0 };
    let row_h = ROW_H + 6;
    let list = rows(t);
    let fits = (lh / row_h).max(1) as usize;
    let max = list.len().saturating_sub(fits);
    let top = st.scroll.min(max);
    if list.is_empty() {
        g.text(
            st.t(
                "ningún proceso tiene sockets de red",
                "no process holds a network socket",
            ),
            x,
            cy + 4,
            p.dim,
        );
    }
    // Columns: verdict, pid, program, proto, local, remote; an alert's button
    // over the remote one, which a listener does not have. The pid goes first
    // when the window is too narrow for all of them.
    let bw = 11 * GLYPH_W;
    let show_pid = w >= 70 * GLYPH_W;
    let c_pid = x + 10 * GLYPH_W;
    let c_prog = c_pid + if show_pid { 7 * GLYPH_W } else { 0 };
    let c_proto = c_prog + 14 * GLYPH_W;
    let c_local = c_proto + 6 * GLYPH_W;
    let c_remote = c_local + 23 * GLYPH_W;
    for (row, (i, s)) in list.iter().enumerate().skip(top).take(fits).enumerate() {
        let ry = cy + row as i32 * row_h;
        let ty = ry + (ROW_H - GLYPH_H) / 2 + 2;
        let (tag, c) = match s.verdict {
            Verdict::Alert => (st.t("¡ALERTA!", "ALERT!"), CRIT),
            Verdict::Listen => (st.t("escucha", "listen"), OK),
            Verdict::Connected => (st.t("conexión", "connected"), p.dim),
            Verdict::Idle => ("-", p.dim),
        };
        if s.verdict == Verdict::Alert {
            g.cv.round_rect_a(x - 6, ry, w + 12, ROW_H + 4, 6, CRIT, 0.18);
        } else if row % 2 == 0 {
            g.cv.round_rect(x - 6, ry, w + 12, ROW_H + 4, 6, p.field);
        }
        g.bold(tag, x, ty, c);
        if show_pid {
            g.text(&s.pid.to_string(), c_pid, ty, p.dim);
        }
        g.text(&fit(s.short_program(), 13 * GLYPH_W), c_prog, ty, p.text);
        g.text(&s.proto, c_proto, ty, p.dim);
        g.text(&fit_left(&s.local, 22 * GLYPH_W), c_local, ty, p.text);
        if let Some(remote) = &s.remote {
            g.text(
                &fit_left(remote, (x + w - c_remote).max(GLYPH_W)),
                c_remote,
                ty,
                p.dim,
            );
        }
        if s.verdict == Verdict::Alert {
            let act = (!sealed).then_some(Act::ExpectPort(i)).and_then(free);
            g.button(
                x + w - bw,
                ry,
                bw,
                st.t("Permitir", "Allow"),
                Style::Normal,
                act,
            );
        }
    }

    // ── the rules ──
    let ry0 = y + h - rules_h - if show_console { console_h + 10 } else { 0 } + 8;
    g.section(
        x,
        ry0,
        w,
        st.t(
            "Puertos esperados (/etc/hunter/ports)",
            "Expected ports (/etc/hunter/ports)",
        ),
    );
    let fbw = 10 * GLYPH_W;
    for (i, rule) in t.rules.iter().take(4).enumerate() {
        let ry = ry0 + 26 + i as i32 * (ROW_H + 6);
        let shown = rule.strip_prefix("expect ").unwrap_or(rule);
        g.text(&fit_left(shown, w - fbw - 16), x, ry + 5, p.text);
        g.button(
            x + w - fbw,
            ry,
            fbw,
            st.t("Quitar", "Remove"),
            Style::Normal,
            free(Act::ForgetRule(i)),
        );
    }
    if t.rules.len() > 4 {
        let more = format!(
            "+{} {}",
            t.rules.len() - 4,
            st.t("más (hunterctl net)", "more (hunterctl net)")
        );
        g.text(&more, x + w / 2, ry0, p.dim);
    }
    if t.reported > 0 {
        let s = format!(
            "{}: {}",
            st.t("avisos desde el arranque", "alerts since boot"),
            t.reported
        );
        let sw = lunarbar::draw::Canvas::text_width(&s);
        if t.rules.len() <= 4 {
            g.text(&s, x + w - sw, ry0, WARN);
        }
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
