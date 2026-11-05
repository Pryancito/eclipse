//! lunarnet — the network settings: each interface's IPv4 (DHCP, manual or
//! off), IPv6 (automatic, DHCPv6 or manual) and DNS servers, beside what the
//! interface has right now.
//!
//! The window edits `/etc/eclipse/network` (`lunarbar::netconf`) and checks it
//! as it is typed: a field that does not make sense says why, in red, and
//! "Aplicar" refuses until it does. Applying saves the file and tells the
//! network service to read it again.
//!
//! `lunarnet --service` is that service. eclipse-init runs it (through
//! `eclipse-udhcpc`, the `udhcpc` service the image always had) at boot: it
//! sets the static addresses and routes with `ip`, runs `udhcpc` / `udhcpc6`
//! for the interfaces that want DHCP, writes `resolv.conf` when the servers are
//! fixed, and on SIGHUP stops all of it and starts again from the file.
//!
//! Tab and Shift+Tab move between the fields, ←/→ change a mode, ↑/↓ the
//! interface, Intro applies and Esc closes.
//!
//! `--dump PATH:WxH` renders a made-up machine to a raw ARGB8888 file.

use std::os::fd::{AsFd, AsRawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::i18n::{Key, Lang};
use lunarbar::keys::{
    KEY_BACKSPACE_WL, KEY_DOWN_WL, KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL, KEY_LEFT_WL,
    KEY_RIGHT_WL, KEY_TAB_WL, KEY_UP_WL,
};
use lunarbar::netconf::{self, Field, Iface, Live, Problem, V4, V6};
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

// ── look ─────────────────────────────────────────────────────────────────────

const GROUND: Rgb = (0x04, 0x07, 0x0d);
const SURFACE: Rgb = (0x0a, 0x14, 0x23);
const WELL: Rgb = (0x06, 0x0d, 0x18);
const HAIRLINE: Rgb = (0x1d, 0x35, 0x50);
const TEXT: Rgb = (0xe6, 0xf1, 0xff);
const MUTED: Rgb = (0x8a, 0xa4, 0xc2);
const DIM: Rgb = (0x4a, 0x60, 0x7a);
const CYAN: Rgb = (0x38, 0xe1, 0xff);
const GOLD: Rgb = (0xf2, 0xc1, 0x4e);
const WARN: Rgb = (0xff, 0x8a, 0x5c);

const MARGIN: i32 = 48;
const PANEL_MAX_W: i32 = 960;
const HEAD_H: i32 = 48;
const FOOT_H: i32 = 56;
const PAD: i32 = 16;
const LIST_W: i32 = 232;
const IFACE_H: i32 = 76;
const SEC_HEAD_H: i32 = 32;
const ROW_H: i32 = 36;
const FIELD_H: i32 = 28;
const SEG_W: i32 = 128;
const LABEL_W: i32 = 192;
const FIELD_MAX_W: i32 = 272;
const SEC_GAP: i32 = 16;
const BTN_W: i32 = 120;
const BTN_H: i32 = 32;

/// The longest text a field takes: a full IPv6 address with its prefix is 43.
const MAX_TYPED: usize = 48;
/// The DNS line holds a few addresses.
const MAX_DNS: usize = 160;

const PIDFILE: &str = "/run/lunarnet.pid";
/// Present while the DNS servers are fixed: the udhcpc scripts leave
/// `resolv.conf` alone then, so a lease does not overwrite them.
const DNS_MARK: &str = "/run/lunarnet/dns";

// ── what has the focus ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    List,
    Mode4,
    Addr4,
    Gw4,
    Mode6,
    Addr6,
    Gw6,
    Dns,
    Revert,
    Apply,
}

const ORDER: [Focus; 10] = [
    Focus::List,
    Focus::Mode4,
    Focus::Addr4,
    Focus::Gw4,
    Focus::Mode6,
    Focus::Addr6,
    Focus::Gw6,
    Focus::Dns,
    Focus::Revert,
    Focus::Apply,
];

impl Focus {
    fn field(self) -> Option<Field> {
        Some(match self {
            Focus::Addr4 => Field::Address4,
            Focus::Gw4 => Field::Gateway4,
            Focus::Addr6 => Field::Address6,
            Focus::Gw6 => Field::Gateway6,
            Focus::Dns => Field::Dns,
            _ => return None,
        })
    }

    /// Whether `i` lets this be typed in: an address only in manual mode.
    fn enabled(self, i: &Iface) -> bool {
        match self {
            Focus::Addr4 | Focus::Gw4 => i.v4 == V4::Static,
            Focus::Addr6 | Focus::Gw6 => i.v6 == V6::Static,
            _ => true,
        }
    }
}

fn text_of(i: &Iface, f: Field) -> &str {
    match f {
        Field::Address4 => &i.address4,
        Field::Gateway4 => &i.gateway4,
        Field::Address6 => &i.address6,
        Field::Gateway6 => &i.gateway6,
        Field::Dns => &i.dns,
    }
}

fn text_mut(i: &mut Iface, f: Field) -> &mut String {
    match f {
        Field::Address4 => &mut i.address4,
        Field::Gateway4 => &mut i.gateway4,
        Field::Address6 => &mut i.address6,
        Field::Gateway6 => &mut i.gateway6,
        Field::Dns => &mut i.dns,
    }
}

/// The next (or previous) place Tab goes from `f`, skipping what `i` does
/// not let be typed in.
fn step(f: Focus, i: &Iface, back: bool) -> Focus {
    let at = ORDER.iter().position(|x| *x == f).unwrap_or(0);
    let n = ORDER.len();
    (1..=n)
        .map(|k| ORDER[if back { (at + n - k) % n } else { (at + k) % n }])
        .find(|x| x.enabled(i))
        .unwrap_or(Focus::List)
}

/// What a key types into an address field. Only what an address or a list of
/// them is made of: digits, the hex letters, `.`, `:`, `/`, the space and the
/// comma. `:` is Shift+. on a Spanish keyboard and Shift+; on a US one, `/`
/// Shift+7 or the US slash key, and the keypad works too.
fn field_char(code: u32, shift: bool) -> Option<char> {
    Some(match (code, shift) {
        (52, true) | (39, true) => ':',
        (8, true) | (53, _) | (98, _) => '/',
        (2..=10, false) => (b'1' + (code - 2) as u8) as char,
        (11, false) | (82, _) => '0',
        (79..=81, _) => (b'1' + (code - 79) as u8) as char,
        (75..=77, _) => (b'4' + (code - 75) as u8) as char,
        (71..=73, _) => (b'7' + (code - 71) as u8) as char,
        (52, false) | (83, _) => '.',
        (51, false) => ',',
        (57, _) => ' ',
        (30, s) => pick('a', s),
        (48, s) => pick('b', s),
        (46, s) => pick('c', s),
        (32, s) => pick('d', s),
        (18, s) => pick('e', s),
        (33, s) => pick('f', s),
        _ => return None,
    })
}

fn pick(c: char, upper: bool) -> char {
    if upper {
        c.to_ascii_uppercase()
    } else {
        c
    }
}

// ── layout ───────────────────────────────────────────────────────────────────

type Rect = (i32, i32, i32, i32);

fn hit(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

/// Where everything is, for drawing and for the pointer alike.
struct Layout {
    panel: Rect,
    /// Left of the settings column, and its width.
    rx: i32,
    rw: i32,
    sec4: i32,
    sec6: i32,
    secdns: i32,
    seg4: [Rect; 3],
    seg6: [Rect; 3],
    addr4: Rect,
    gw4: Rect,
    addr6: Rect,
    gw6: Rect,
    dns: Rect,
    revert: Rect,
    apply: Rect,
}

/// The height of a mode section: its heading, the mode row, two fields.
const SEC_H: i32 = SEC_HEAD_H + 3 * ROW_H;
const DNS_H: i32 = SEC_HEAD_H + ROW_H + GLYPH_H + 8;
const BODY_H: i32 = 2 * SEC_H + DNS_H + 2 * SEC_GAP + 2 * PAD;

fn layout(w: i32, h: i32, n: usize) -> Layout {
    let pw = PANEL_MAX_W.min(w - 2 * MARGIN).max(480.min(w));
    let list_h = n.max(1) as i32 * IFACE_H + 2 * PAD;
    let ph = HEAD_H + BODY_H.max(list_h) + FOOT_H;
    let (px, py) = ((w - pw) / 2, ((h - ph) / 2).max(0));
    let rx = px + LIST_W + PAD;
    let rw = pw - LIST_W - 2 * PAD;
    let sec4 = py + HEAD_H + PAD;
    let sec6 = sec4 + SEC_H + SEC_GAP;
    let secdns = sec6 + SEC_H + SEC_GAP;
    let fx = rx + LABEL_W;
    let fw = FIELD_MAX_W.min(rw - LABEL_W).max(96);
    let row = |top: i32, k: i32| top + SEC_HEAD_H + k * ROW_H + (ROW_H - FIELD_H) / 2;
    let seg = |top: i32| -> [Rect; 3] {
        let y = row(top, 0);
        let sw = SEG_W.min((rw - LABEL_W) / 3).max(48);
        [0, 1, 2].map(|k| (fx + k * sw, y, sw, FIELD_H))
    };
    let foot = py + ph - FOOT_H + (FOOT_H - BTN_H) / 2;
    let apply = (px + pw - PAD - BTN_W, foot, BTN_W, BTN_H);
    Layout {
        panel: (px, py, pw, ph),
        rx,
        rw,
        sec4,
        sec6,
        secdns,
        seg4: seg(sec4),
        seg6: seg(sec6),
        addr4: (fx, row(sec4, 1), fw, FIELD_H),
        gw4: (fx, row(sec4, 2), fw, FIELD_H),
        addr6: (fx, row(sec6, 1), fw, FIELD_H),
        gw6: (fx, row(sec6, 2), fw, FIELD_H),
        dns: (fx, row(secdns, 0), (rw - LABEL_W).max(96), FIELD_H),
        revert: (apply.0 - PAD / 2 - BTN_W, foot, BTN_W, BTN_H),
        apply,
    }
}

fn iface_row(l: &Layout, k: usize) -> Rect {
    let (px, py, _, _) = l.panel;
    (px + 1, py + HEAD_H + PAD + k as i32 * IFACE_H, LIST_W - 1, IFACE_H)
}

fn field_rect(l: &Layout, f: Field) -> Rect {
    match f {
        Field::Address4 => l.addr4,
        Field::Gateway4 => l.gw4,
        Field::Address6 => l.addr6,
        Field::Gateway6 => l.gw6,
        Field::Dns => l.dns,
    }
}

// ── what is drawn ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Plain,
    Good,
    Bad,
}

struct Scene<'a> {
    lang: Lang,
    live: &'a [Live],
    edit: &'a [Iface],
    /// Which of `edit` differ from the file.
    dirty: &'a [bool],
    dns_now: &'a [String],
    sel: usize,
    focus: Focus,
    status: &'a str,
    tone: Tone,
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

fn frame(cv: &mut Canvas, r: Rect, c: Rgb) {
    let (x, y, w, h) = r;
    for (a, b, rw, rh) in [(x, y, w, 1), (x, y + h - 1, w, 1), (x, y, 1, h), (x + w - 1, y, 1, h)] {
        cv.fill_rect_a(a, b, rw, rh, c, 1.0);
    }
}

fn state_word(state: &str, lang: Lang) -> (&'static str, Rgb) {
    match state {
        "up" => (lang.s(Key::net_state_up), CYAN),
        "dormant" => (lang.s(Key::net_state_dormant), GOLD),
        "down" | "lowerlayerdown" => (lang.s(Key::net_state_down), WARN),
        _ => (lang.s(Key::net_state_unknown), MUTED),
    }
}

fn draw(cv: &mut Canvas, w: i32, h: i32, s: &Scene) {
    let l = layout(w, h, s.live.len());
    let (px, py, pw, ph) = l.panel;
    cv.fill_rect_a(0, 0, w, h, GROUND, 0.9);
    cv.fill_rect_a(px, py, pw, ph, SURFACE, 1.0);
    frame(cv, l.panel, HAIRLINE);
    cv.brackets(px, py, pw, ph, 16, 2, GOLD);

    // Head.
    let hy = py + (HEAD_H - GLYPH_H) / 2;
    let tw = cv.text_bold(s.lang.s(Key::net_title), px + PAD, hy, CYAN);
    cv.text(s.lang.s(Key::net_subtitle), px + PAD + tw + 12, hy, MUTED);
    let keys = s.lang.s(Key::net_keys);
    let kw = Canvas::text_width(keys);
    if kw < pw - 2 * PAD - 220 {
        cv.text(keys, px + pw - PAD - kw, hy, MUTED);
    }
    cv.hline(px + PAD, py + HEAD_H - 1, pw - 2 * PAD, HAIRLINE, 1.0);

    // The interfaces.
    let body_h = ph - HEAD_H - FOOT_H;
    cv.fill_rect_a(px + LIST_W, py + HEAD_H + PAD, 1, body_h - 2 * PAD, HAIRLINE, 1.0);
    if s.live.is_empty() {
        cv.text(s.lang.s(Key::net_no_ifaces), px + PAD, py + HEAD_H + PAD + 8, MUTED);
    }
    for (k, lv) in s.live.iter().enumerate() {
        let r = iface_row(&l, k);
        let on = k == s.sel;
        if on {
            cv.fill_rect_a(r.0, r.1, r.2, r.3, CYAN, 0.08);
            cv.fill_rect_a(r.0, r.1 + 8, 2, r.3 - 16, CYAN, 1.0);
        }
        if on && s.focus == Focus::List {
            cv.brackets(r.0 + 4, r.1 + 2, r.2 - 8, r.3 - 4, 8, 1, GOLD);
        }
        let x = px + PAD;
        let y1 = r.1 + 10;
        cv.text_bold(&fit(&lv.name, 112), x, y1, if on { TEXT } else { MUTED });
        let (word, col) = state_word(&lv.state, s.lang);
        let ww = Canvas::text_width(word);
        cv.fill_rect_a(px + LIST_W - PAD - ww - 12, y1 + (GLYPH_H - 6) / 2, 6, 6, col, 1.0);
        cv.text(word, px + LIST_W - PAD - ww, y1, col);
        let mac = if lv.mac.is_empty() { "—" } else { &lv.mac };
        cv.text(mac, x, y1 + GLYPH_H + 6, DIM);
        if s.dirty.get(k).copied().unwrap_or(false) {
            let t = s.lang.s(Key::net_unapplied);
            cv.text(t, x, y1 + 2 * (GLYPH_H + 6), GOLD);
        }
    }

    let Some(i) = s.edit.get(s.sel) else {
        draw_foot(cv, &l, s, false);
        return;
    };
    let lv = &s.live[s.sel.min(s.live.len().saturating_sub(1))];
    let problems = netconf::check(i);
    let why = |f: Field| problems.iter().find(|p| p.field == f).map(|p| p.why);

    // IPv4.
    let now4 = {
        let mut t = if lv.v4.is_empty() {
            s.lang.s(Key::net_no_address).to_string()
        } else {
            lv.v4.join(" ")
        };
        if let Some(g) = &lv.gateway4 {
            t += &format!(" · {} {g}", s.lang.s(Key::net_via));
        }
        t
    };
    section(cv, &l, l.sec4, "IPV4", &now4, s.lang);
    let modes4 = V4::ALL.map(|m| {
        s.lang.s(match m {
            V4::Dhcp => Key::net_mode_dhcp,
            V4::Static => Key::net_mode_manual,
            V4::Off => Key::net_mode_off,
        })
    });
    let at4 = V4::ALL.iter().position(|m| *m == i.v4).unwrap_or(0);
    segments(cv, &l, l.sec4, &l.seg4, &modes4, at4, s.focus == Focus::Mode4, s.lang);
    // The static placeholders are EXAMPLE ADDRESSES, not prose: the same in
    // both languages but for the parenthesis, which is a word.
    let optional4 = format!("192.168.1.1  ({})", s.lang.s(Key::net_optional));
    let (ph4, pg4) = match i.v4 {
        V4::Static => ("192.168.1.20/24", optional4.as_str()),
        V4::Dhcp => (s.lang.s(Key::net_ph_dhcp), s.lang.s(Key::net_ph_dhcp)),
        V4::Off => (s.lang.s(Key::net_ph_no_v4), s.lang.s(Key::net_ph_no_v4)),
    };
    let addr_label = s.lang.s(Key::net_addr_prefix);
    let gw_label = s.lang.s(Key::net_gateway);
    field(cv, &l, s, i, Focus::Addr4, addr_label, ph4, why(Field::Address4));
    field(cv, &l, s, i, Focus::Gw4, gw_label, pg4, why(Field::Gateway4));

    // IPv6.
    let now6 = {
        let mut t = if lv.v6.is_empty() {
            s.lang.s(Key::net_no_address).to_string()
        } else {
            lv.v6.join(" ")
        };
        if let Some(g) = &lv.gateway6 {
            t += &format!(" · {} {g}", s.lang.s(Key::net_via));
        }
        t
    };
    section(cv, &l, l.sec6, "IPV6", &now6, s.lang);
    let modes6 = V6::ALL.map(|m| {
        s.lang.s(match m {
            V6::Auto => Key::net_mode_auto,
            V6::Dhcp => Key::net_mode_dhcpv6,
            V6::Static => Key::net_mode_manual,
        })
    });
    let at6 = V6::ALL.iter().position(|m| *m == i.v6).unwrap_or(0);
    segments(cv, &l, l.sec6, &l.seg6, &modes6, at6, s.focus == Focus::Mode6, s.lang);
    let optional6 = format!("fe80::1  ({})", s.lang.s(Key::net_optional));
    let (ph6, pg6) = match i.v6 {
        V6::Static => ("2001:db8::20/64", optional6.as_str()),
        V6::Auto => (s.lang.s(Key::net_ph_slaac), s.lang.s(Key::net_ph_router)),
        V6::Dhcp => (s.lang.s(Key::net_ph_dhcpv6), s.lang.s(Key::net_ph_router)),
    };
    field(cv, &l, s, i, Focus::Addr6, addr_label, ph6, why(Field::Address6));
    field(cv, &l, s, i, Focus::Gw6, gw_label, pg6, why(Field::Gateway6));

    // DNS.
    let dns_now = if s.dns_now.is_empty() {
        s.lang.s(Key::net_dns_none).to_string()
    } else {
        s.dns_now.join(" ")
    };
    section(cv, &l, l.secdns, "DNS", &dns_now, s.lang);
    field(
        cv,
        &l,
        s,
        i,
        Focus::Dns,
        s.lang.s(Key::net_dns_servers),
        s.lang.s(Key::net_dns_ph),
        why(Field::Dns),
    );
    let hint = s.lang.s(Key::net_dns_hint);
    cv.text(&fit(hint, l.rw - LABEL_W), l.dns.0, l.dns.1 + FIELD_H + 6, DIM);

    draw_foot(cv, &l, s, s.dirty.get(s.sel).copied().unwrap_or(false));
}

fn section(cv: &mut Canvas, l: &Layout, top: i32, title: &str, now: &str, lang: Lang) {
    let y = top + (SEC_HEAD_H - GLYPH_H) / 2;
    let tw = cv.text_bold(title, l.rx, y, CYAN);
    cv.hline(l.rx, top + SEC_HEAD_H - 4, l.rw, HAIRLINE, 0.6);
    let label = lang.s(Key::net_now);
    let room = l.rw - tw - 24;
    let lw = Canvas::text_width(label);
    let val = fit(now, room - lw);
    let vw = Canvas::text_width(&val);
    let x = l.rx + l.rw - vw;
    cv.text(label, x - lw, y, DIM);
    cv.text(&val, x, y, TEXT);
}

#[allow(clippy::too_many_arguments)]
fn segments(
    cv: &mut Canvas,
    l: &Layout,
    top: i32,
    segs: &[Rect; 3],
    names: &[&str; 3],
    at: usize,
    focused: bool,
    lang: Lang,
) {
    let y = top + SEC_HEAD_H + (ROW_H - GLYPH_H) / 2;
    cv.text(lang.s(Key::net_mode), l.rx, y, MUTED);
    for (k, r) in segs.iter().enumerate() {
        let on = k == at;
        if on {
            cv.fill_rect_a(r.0, r.1, r.2, r.3, CYAN, 0.16);
        } else {
            cv.fill_rect_a(r.0, r.1, r.2, r.3, WELL, 1.0);
        }
        frame(cv, *r, if on { CYAN } else { HAIRLINE });
        let t = fit(names[k], r.2 - 8);
        let tx = r.0 + (r.2 - Canvas::text_width(&t)) / 2;
        let ty = r.1 + (r.3 - GLYPH_H) / 2;
        if on {
            cv.text_bold(&t, tx, ty, CYAN);
        } else {
            cv.text(&t, tx, ty, MUTED);
        }
    }
    if focused {
        let (x0, y0) = (segs[0].0, segs[0].1);
        let x1 = segs[2].0 + segs[2].2;
        cv.brackets(x0 - 4, y0 - 4, x1 - x0 + 8, FIELD_H + 8, 8, 2, GOLD);
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn field(
    cv: &mut Canvas,
    l: &Layout,
    s: &Scene,
    i: &Iface,
    f: Focus,
    label: &str,
    hint: &str,
    why: Option<Key>,
) {
    let Some(which) = f.field() else { return };
    let r = field_rect(l, which);
    let enabled = f.enabled(i);
    let focused = s.focus == f;
    cv.text(label, l.rx, r.1 + (FIELD_H - GLYPH_H) / 2, if enabled { MUTED } else { DIM });
    let text = text_of(i, which);
    let ty = r.1 + (FIELD_H - GLYPH_H) / 2;
    if !enabled {
        cv.fill_rect_a(r.0, r.1, r.2, r.3, SURFACE, 1.0);
        frame(cv, r, (0x13, 0x24, 0x38));
        cv.text(&fit(hint, r.2 - 16), r.0 + 8, ty, DIM);
        return;
    }
    cv.fill_rect_a(r.0, r.1, r.2, r.3, WELL, 1.0);
    let edge = match (why.is_some(), focused) {
        (true, _) => WARN,
        (false, true) => CYAN,
        _ => HAIRLINE,
    };
    frame(cv, r, edge);
    if focused {
        cv.fill_rect_a(r.0, r.1 + r.3 - 2, r.2, 2, edge, 1.0);
    }
    if text.is_empty() && !focused {
        cv.text(&fit(hint, r.2 - 16), r.0 + 8, ty, DIM);
    } else {
        // The end of a long value is what is being typed: show that end.
        let cols = ((r.2 - 24) / GLYPH_W).max(1) as usize;
        let n = text.chars().count();
        let shown: String = text.chars().skip(n.saturating_sub(cols)).collect();
        let adv = cv.text(&shown, r.0 + 8, ty, TEXT);
        if focused {
            cv.fill_rect_a(r.0 + 8 + adv + 1, r.1 + 6, 2, FIELD_H - 12, CYAN, 1.0);
        }
    }
    if let Some(w) = why {
        let x = r.0 + r.2 + 12;
        let room = l.rx + l.rw - x;
        // With no room beside it, "Aplicar" still says it on the status line.
        if room >= 8 * GLYPH_W {
            cv.text(&fit(s.lang.s(w), room), x, ty, WARN);
        }
    }
}

fn button(cv: &mut Canvas, r: Rect, label: &str, lit: bool, focused: bool) {
    cv.fill_rect_a(r.0, r.1, r.2, r.3, if lit { GOLD } else { WELL }, if lit { 0.14 } else { 1.0 });
    frame(cv, r, if lit { GOLD } else { HAIRLINE });
    let tx = r.0 + (r.2 - Canvas::text_width(label)) / 2;
    let ty = r.1 + (r.3 - GLYPH_H) / 2;
    if lit {
        cv.text_bold(label, tx, ty, GOLD);
    } else {
        cv.text(label, tx, ty, MUTED);
    }
    if focused {
        cv.brackets(r.0 - 4, r.1 - 4, r.2 + 8, r.3 + 8, 8, 2, CYAN);
    }
}

fn draw_foot(cv: &mut Canvas, l: &Layout, s: &Scene, dirty: bool) {
    let (px, py, pw, ph) = l.panel;
    let top = py + ph - FOOT_H;
    cv.hline(px + PAD, top, pw - 2 * PAD, HAIRLINE, 1.0);
    button(cv, l.revert, s.lang.s(Key::net_undo_caps), false, s.focus == Focus::Revert);
    button(cv, l.apply, s.lang.s(Key::net_apply_caps), dirty, s.focus == Focus::Apply);
    let col = match s.tone {
        Tone::Plain => MUTED,
        Tone::Good => CYAN,
        Tone::Bad => WARN,
    };
    let room = l.revert.0 - px - 2 * PAD;
    cv.text(&fit(s.status, room), px + PAD, top + (FOOT_H - GLYPH_H) / 2, col);
}

// ── the live state ───────────────────────────────────────────────────────────

fn run(args: &[&str]) -> String {
    std::process::Command::new(args[0])
        .args(&args[1..])
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn read_live() -> Vec<Live> {
    netconf::live(
        Path::new("/sys/class/net"),
        &run(&["ip", "-o", "addr"]),
        &run(&["ip", "-4", "route"]),
        &run(&["ip", "-6", "route"]),
    )
}

fn read_dns() -> Vec<String> {
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver").map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
        .collect()
}

/// The service's process, when it is running: the pid in its file, if that
/// process is a lunarnet.
fn service_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(PIDFILE).ok()?.trim().parse().ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    (pid > 1 && comm.trim() == "lunarnet").then_some(pid)
}

// ── the editor, without Wayland ──────────────────────────────────────────────

/// Everything the window holds, so the keys and the pointer can be tested
/// without a compositor.
struct Editor {
    /// What the file says.
    saved: Vec<Iface>,
    live: Vec<Live>,
    /// One per `live`, as edited.
    edit: Vec<Iface>,
    dns_now: Vec<String>,
    sel: usize,
    focus: Focus,
    status: String,
    tone: Tone,
    /// Read once, at startup, like every other client: the session's language
    /// does not change under a window that is already open (`eclipse-locale`
    /// reconfigures labwc, it does not reach into running clients).
    lang: Lang,
}

impl Editor {
    fn new(saved: Vec<Iface>, live: Vec<Live>, dns_now: Vec<String>) -> Self {
        let edit = live.iter().map(|l| netconf::settings_for(&saved, &l.name)).collect();
        let lang = Lang::current();
        Editor {
            saved,
            live,
            edit,
            dns_now,
            sel: 0,
            focus: Focus::List,
            status: lang.s(Key::net_saved_in).into(),
            tone: Tone::Plain,
            lang,
        }
    }

    fn dirty(&self) -> Vec<bool> {
        self.edit.iter().map(|i| *i != netconf::settings_for(&self.saved, &i.name)).collect()
    }

    fn scene(&self) -> Scene<'_> {
        Scene {
            lang: self.lang,
            live: &self.live,
            edit: &self.edit,
            dirty: &[],
            dns_now: &self.dns_now,
            sel: self.sel,
            focus: self.focus,
            status: &self.status,
            tone: self.tone,
        }
    }

    /// A fresh look at the interfaces: an interface that came or went is
    /// added or dropped, and what is being typed stays.
    fn refresh(&mut self, live: Vec<Live>, dns_now: Vec<String>) -> bool {
        let changed = live != self.live || dns_now != self.dns_now;
        let name = self.edit.get(self.sel).map(|i| i.name.clone());
        let old = std::mem::take(&mut self.edit);
        self.edit = live
            .iter()
            .map(|l| {
                old.iter()
                    .find(|i| i.name == l.name)
                    .cloned()
                    .unwrap_or_else(|| netconf::settings_for(&self.saved, &l.name))
            })
            .collect();
        self.sel = name
            .and_then(|n| live.iter().position(|l| l.name == n))
            .unwrap_or(0)
            .min(live.len().saturating_sub(1));
        self.live = live;
        self.dns_now = dns_now;
        changed
    }

    fn cur(&mut self) -> Option<&mut Iface> {
        self.edit.get_mut(self.sel)
    }

    fn select(&mut self, k: usize) {
        if k < self.edit.len() {
            self.sel = k;
        }
    }

    fn cycle_mode(&mut self, f: Focus, delta: i32) {
        let Some(i) = self.cur() else { return };
        let turn = |at: usize| (at as i32 + delta).rem_euclid(3) as usize;
        match f {
            Focus::Mode4 => {
                let at = V4::ALL.iter().position(|m| *m == i.v4).unwrap_or(0);
                i.v4 = V4::ALL[turn(at)];
            }
            Focus::Mode6 => {
                let at = V6::ALL.iter().position(|m| *m == i.v6).unwrap_or(0);
                i.v6 = V6::ALL[turn(at)];
            }
            _ => {}
        }
    }

    fn type_char(&mut self, c: char) {
        let Some(which) = self.focus.field() else { return };
        let f = self.focus;
        let Some(i) = self.cur() else { return };
        if !f.enabled(i) {
            return;
        }
        let max = if which == Field::Dns { MAX_DNS } else { MAX_TYPED };
        let t = text_mut(i, which);
        if t.chars().count() < max {
            t.push(c);
        }
    }

    fn backspace(&mut self) {
        let Some(which) = self.focus.field() else { return };
        if let Some(i) = self.cur() {
            text_mut(i, which).pop();
        }
    }

    fn revert(&mut self) {
        if let Some(name) = self.edit.get(self.sel).map(|i| i.name.clone()) {
            self.edit[self.sel] = netconf::settings_for(&self.saved, &name);
            self.status = format!("{name}: {}", self.lang.s(Key::net_reverted));
            self.tone = Tone::Plain;
        }
    }

    /// The file as it would be saved: every interface it already named, plus
    /// every edited one that is not the default.
    fn merged(&self) -> Vec<Iface> {
        let mut out = self.saved.clone();
        for i in &self.edit {
            match out.iter_mut().find(|o| o.name == i.name) {
                Some(o) => *o = i.clone(),
                None if *i != Iface::new(&i.name) => out.push(i.clone()),
                None => {}
            }
        }
        out
    }

    /// The first edited interface something is wrong with, and the problem.
    fn first_problem(&self) -> Option<(usize, Problem)> {
        self.edit
            .iter()
            .enumerate()
            .find_map(|(k, i)| netconf::check(i).into_iter().next().map(|p| (k, p)))
    }

    /// Check, save, and tell the service. `save` and `signal` are the outside
    /// world, passed in so the tests do not write `/etc`.
    fn apply(
        &mut self,
        save: impl FnOnce(&[Iface]) -> std::io::Result<()>,
        signal: impl FnOnce() -> bool,
    ) {
        if let Some((k, p)) = self.first_problem() {
            self.sel = k;
            self.focus = match p.field {
                Field::Address4 => Focus::Addr4,
                Field::Gateway4 => Focus::Gw4,
                Field::Address6 => Focus::Addr6,
                Field::Gateway6 => Focus::Gw6,
                Field::Dns => Focus::Dns,
            };
            self.status = format!("{}: {}", self.edit[k].name, self.lang.s(p.why));
            self.tone = Tone::Bad;
            return;
        }
        let all = self.merged();
        if let Err(e) = save(&all) {
            self.status = format!("{}: {e}", self.lang.s(Key::net_save_failed));
            self.tone = Tone::Bad;
            return;
        }
        self.saved = all;
        if signal() {
            self.status = self.lang.s(Key::net_saved_applied).into();
            self.tone = Tone::Good;
        } else {
            self.status = self.lang.s(Key::net_saved_no_service).into();
            self.tone = Tone::Bad;
        }
    }

    /// One key. Returns false when the window should close.
    fn key(&mut self, code: u32, shift: bool) -> bool {
        let Some(i) = self.edit.get(self.sel).cloned() else {
            return code != KEY_ESC_WL;
        };
        match code {
            KEY_ESC_WL => return false,
            KEY_TAB_WL => self.focus = step(self.focus, &i, shift),
            KEY_UP_WL | KEY_DOWN_WL if self.focus == Focus::List => {
                let k = if code == KEY_UP_WL { self.sel.saturating_sub(1) } else { self.sel + 1 };
                self.select(k);
            }
            KEY_UP_WL => self.focus = step(self.focus, &i, true),
            KEY_DOWN_WL => self.focus = step(self.focus, &i, false),
            KEY_LEFT_WL | KEY_RIGHT_WL if matches!(self.focus, Focus::Mode4 | Focus::Mode6) => {
                self.cycle_mode(self.focus, if code == KEY_LEFT_WL { -1 } else { 1 });
            }
            KEY_LEFT_WL | KEY_RIGHT_WL if matches!(self.focus, Focus::Revert | Focus::Apply) => {
                self.focus = if self.focus == Focus::Apply { Focus::Revert } else { Focus::Apply };
            }
            KEY_ENTER_WL | KEY_KPENTER_WL => match self.focus {
                Focus::List => self.focus = Focus::Mode4,
                Focus::Revert => self.revert(),
                _ => self.apply(|a| netconf::save(Path::new(netconf::CONF), a), signal_service),
            },
            KEY_BACKSPACE_WL => self.backspace(),
            _ => {
                if let Some(c) = field_char(code, shift) {
                    self.type_char(c);
                }
            }
        }
        true
    }

    /// A click at (x, y) on a `w`×`h` surface. Returns false when the window
    /// should close.
    fn click(&mut self, w: i32, h: i32, x: i32, y: i32) -> bool {
        let l = layout(w, h, self.live.len());
        if !hit(l.panel, x, y) {
            // Outside the panel closes, unless that would lose what was typed.
            return self.dirty().iter().any(|d| *d);
        }
        for k in 0..self.live.len() {
            if hit(iface_row(&l, k), x, y) {
                self.select(k);
                self.focus = Focus::List;
                return true;
            }
        }
        let Some(i) = self.cur() else { return true };
        for (k, r) in l.seg4.iter().enumerate() {
            if hit(*r, x, y) {
                i.v4 = V4::ALL[k];
                self.focus = Focus::Mode4;
                return true;
            }
        }
        for (k, r) in l.seg6.iter().enumerate() {
            if hit(*r, x, y) {
                i.v6 = V6::ALL[k];
                self.focus = Focus::Mode6;
                return true;
            }
        }
        let i = i.clone();
        for f in [Focus::Addr4, Focus::Gw4, Focus::Addr6, Focus::Gw6, Focus::Dns] {
            if f.field().is_some_and(|w| hit(field_rect(&l, w), x, y)) && f.enabled(&i) {
                self.focus = f;
                return true;
            }
        }
        if hit(l.revert, x, y) {
            self.focus = Focus::Revert;
            self.revert();
        } else if hit(l.apply, x, y) {
            self.focus = Focus::Apply;
            self.apply(|a| netconf::save(Path::new(netconf::CONF), a), signal_service);
        }
        true
    }

    fn draw(&self, cv: &mut Canvas, w: i32, h: i32) {
        let dirty = self.dirty();
        // The field being typed in says what is wrong with it there and then,
        // whole, where the line beside it may be cut short.
        let live_why = self.edit.get(self.sel).and_then(|i| {
            let f = self.focus.field()?;
            let p = netconf::check(i).into_iter().find(|p| p.field == f)?;
            Some(format!("{}: {}", i.name, self.lang.s(p.why)))
        });
        let mut s = Scene { dirty: &dirty, ..self.scene() };
        if let Some(w) = &live_why {
            s.status = w;
            s.tone = Tone::Bad;
        }
        draw(cv, w, h, &s);
    }
}

fn signal_service() -> bool {
    match service_pid() {
        Some(pid) => unsafe { libc::kill(pid, libc::SIGHUP) == 0 },
        None => false,
    }
}

// ── the service ──────────────────────────────────────────────────────────────

static RELOAD: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        RELOAD.store(true, Ordering::SeqCst);
    } else {
        STOP.store(true, Ordering::SeqCst);
    }
}

fn script(name: &str) -> String {
    let main = format!("/usr/share/udhcpc/{name}");
    if Path::new(&main).exists() {
        main
    } else {
        format!("/etc/udhcpc/{name}")
    }
}

/// A DHCP client for one interface, in the foreground, that dies with us:
/// a respawned service must not find the last one's clients still running.
fn spawn_client(prog: &str, dev: &str, script_name: &str) -> Option<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(prog);
    c.args(["-i", dev, "-f", "-R", "-s", &script(script_name)]);
    // SAFETY: prctl is async-signal-safe and touches nothing but this child.
    unsafe {
        c.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    c.spawn().map_err(|e| eprintln!("lunarnet: {prog} -i {dev}: {e}")).ok()
}

struct Client {
    prog: &'static str,
    dev: String,
    script: &'static str,
    child: Option<std::process::Child>,
    since: Instant,
}

/// One pass over the file: the commands now, the DHCP clients to keep alive,
/// and the static IPv6 addresses added (the next pass takes them away: the
/// kernel's own SLAAC addresses are not this service's to flush).
fn bring_up(added6: &mut Vec<(String, String)>) -> Vec<Client> {
    for (dev, cidr) in added6.drain(..) {
        let _ = run(&["ip", "-6", "addr", "del", &cidr, "dev", &dev]);
    }
    let conf = netconf::load(Path::new(netconf::CONF));
    let mut names = netconf::interfaces(Path::new("/sys/class/net"));
    if names.is_empty() {
        // The NIC may not have probed yet; eth0 is the first one's name.
        names.push("eth0".into());
    }
    let mut clients = Vec::new();
    let mut dns = Vec::new();
    for n in &names {
        let i = netconf::settings_for(&conf, n);
        let p = netconf::plan(&i);
        for cmd in &p.commands {
            let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
            let _ = run(&args);
            if args.get(1..4) == Some(&["-6", "addr", "add"][..]) {
                added6.push((n.clone(), args[4].to_string()));
            }
        }
        if p.dhcp4 {
            clients.push(Client { prog: "udhcpc", dev: n.clone(), script: "default.script", child: None, since: Instant::now() });
        }
        if p.dhcp6 {
            clients.push(Client { prog: "udhcpc6", dev: n.clone(), script: "default6.script", child: None, since: Instant::now() });
        }
        dns.extend(p.dns);
    }
    if dns.is_empty() {
        let _ = std::fs::remove_file(DNS_MARK);
    } else {
        let _ = std::fs::create_dir_all(Path::new(DNS_MARK).parent().unwrap_or(Path::new("/run")));
        let _ = std::fs::write(DNS_MARK, "");
        let tmp = "/etc/resolv.conf.lunarnet";
        if std::fs::write(tmp, netconf::resolv_conf(&dns)).is_ok() {
            let _ = std::fs::rename(tmp, "/etc/resolv.conf");
        }
    }
    eprintln!("lunarnet: {} interfaz(es), {} cliente(s) DHCP", names.len(), clients.len());
    clients
}

fn service() -> ! {
    let _ = std::fs::write(PIDFILE, format!("{}\n", std::process::id()));
    // SAFETY: the handlers only store to atomics.
    unsafe {
        let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGHUP, h);
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
    let mut added6 = Vec::new();
    loop {
        RELOAD.store(false, Ordering::SeqCst);
        let mut clients = bring_up(&mut added6);
        while !RELOAD.load(Ordering::SeqCst) && !STOP.load(Ordering::SeqCst) {
            for c in &mut clients {
                let dead = match &mut c.child {
                    None => true,
                    Some(ch) => !matches!(ch.try_wait(), Ok(None)),
                };
                // A client that keeps dying is retried every five seconds, not
                // in a loop.
                if dead && (c.child.is_none() || c.since.elapsed() > Duration::from_secs(5)) {
                    if let Some(mut ch) = c.child.take() {
                        let _ = ch.wait();
                    }
                    c.child = spawn_client(c.prog, &c.dev, c.script);
                    c.since = Instant::now();
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        for c in &mut clients {
            if let Some(ch) = &mut c.child {
                unsafe { libc::kill(ch.id() as i32, libc::SIGTERM) };
                let _ = ch.wait();
            }
        }
        if STOP.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(PIDFILE);
            std::process::exit(0);
        }
        eprintln!("lunarnet: SIGHUP, aplicando /etc/eclipse/network de nuevo");
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

    ed: Editor,
    shift: bool,
    ptr: (f64, f64),
    done: bool,
}

impl State {
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
        cv.set_square(true);
        self.ed.draw(&mut cv, self.w as i32, self.h as i32);
        // SAFETY: `map` is the `len` bytes mapped for this surface's buffer.
        let data = unsafe { std::slice::from_raw_parts_mut(self.map, self.len) };
        if cv.blit_argb(data) {
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, self.w as i32, self.h as i32);
            surface.commit();
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
            let Some((map, fd)) = map_shm_pool(geom.frame_size, "lunarnet") else {
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
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => st.layer_shell = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_seat" if st.seat.is_none() => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
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
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
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
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match ev {
            // Shift is the first modifier of every xkb keymap labwc hands out.
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => st.shift = mods_depressed & 1 != 0,
            wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => {
                if !st.ed.key(key, st.shift) {
                    st.done = true;
                }
                st.render();
            }
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
            wl_pointer::Event::Enter { surface_x, surface_y, .. }
            | wl_pointer::Event::Motion { surface_x, surface_y, .. } => st.ptr = (surface_x, surface_y),
            wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => {
                let (x, y) = (st.ptr.0 as i32, st.ptr.1 as i32);
                if !st.ed.click(st.w as i32, st.h as i32, x, y) {
                    st.done = true;
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
wayland_client::delegate_noop!(State: ignore ZwlrLayerShellV1);

// ── main ─────────────────────────────────────────────────────────────────────

fn sample() -> Editor {
    let live = vec![
        Live {
            name: "eth0".into(),
            mac: "52:54:00:12:34:56".into(),
            state: "up".into(),
            v4: vec!["10.0.2.15/24".into()],
            v6: vec!["fec0::5054:ff:fe12:3456/64".into(), "fe80::5054:ff:fe12:3456/64".into()],
            gateway4: Some("10.0.2.2".into()),
            gateway6: Some("fe80::2".into()),
        },
        Live {
            name: "wlan0".into(),
            mac: "a4:c3:f0:6e:81:0b".into(),
            state: "down".into(),
            ..Live::default()
        },
    ];
    let mut ed = Editor::new(Vec::new(), live, vec!["10.0.2.3".into()]);
    let i = &mut ed.edit[0];
    i.v4 = V4::Static;
    i.address4 = "192.168.1.20/24".into();
    i.gateway4 = "192.168.2.1".into();
    i.dns = "1.1.1.1 2606:4700:4700::1111".into();
    ed.focus = Focus::Gw4;
    ed.status = format!("eth0: {}", ed.lang.s(Key::net_bad_gw_offnet));
    ed.tone = Tone::Bad;
    ed
}

fn dump(spec: &str) -> ! {
    let (path, dims) = spec.rsplit_once(':').unwrap_or((spec, "1600x900"));
    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .unwrap_or((1600, 900));
    if fill_guard::check_buffer(w, h).is_err() {
        std::process::exit(1);
    }
    let ed = sample();
    let mut cv = Canvas::try_new(w, h).unwrap_or_else(|| std::process::exit(1));
    cv.set_square(true);
    ed.draw(&mut cv, w as i32, h as i32);
    let mut buf = vec![0u8; w * h * 4];
    if !cv.blit_argb(&mut buf) || std::fs::write(path, &buf).is_err() {
        eprintln!("lunarnet: could not write {path}");
        std::process::exit(1);
    }
    println!("lunarnet: wrote {path} ({w}x{h} ARGB8888)");
    std::process::exit(0);
}

/// A second start closes the first, as the other overlays do.
fn toggle_or_lock() -> Option<std::fs::File> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = Path::new(&dir).join("lunarnet.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .ok()?;
    // SAFETY: flock on a descriptor this process owns.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if let Some(pid) = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
            if pid > 1 {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        std::process::exit(0);
    }
    let _ = std::fs::write(&path, std::process::id().to_string());
    Some(f)
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => {}
        Some("--dump") => dump(&args.next().unwrap_or_else(|| "lunarnet.raw".into())),
        Some("--service") => service(),
        Some(_) => {
            eprintln!("usage: lunarnet [--service | --dump PATH:WxH]");
            std::process::exit(2);
        }
    }
    let _lock = toggle_or_lock();
    let mut st = State {
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
        ed: Editor::new(netconf::load(Path::new(netconf::CONF)), read_live(), read_dns()),
        shift: false,
        ptr: (0.0, 0.0),
        done: false,
    };
    let Ok(conn) = lunarbar::wl::connect() else {
        eprintln!("lunarnet: no Wayland compositor found");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarnet: wayland roundtrip failed");
            std::process::exit(1);
        }
    }
    let (Some(comp), Some(ls)) = (st.compositor.clone(), st.layer_shell.clone()) else {
        eprintln!("lunarnet: compositor lacks wlr-layer-shell");
        std::process::exit(1);
    };
    let surface = comp.create_surface(&qh, ());
    let layer = ls.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Overlay, "network".into(), &qh, ());
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();
    st.surface = Some(surface);
    st.layer = Some(layer);

    // Events, and every two seconds a fresh look at the interfaces: a lease
    // that arrives after "Aplicar" shows up without touching anything.
    let mut last = Instant::now();
    while !st.done {
        let _ = conn.flush();
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut pfd, 1, 2000) };
            if n > 0 && pfd.revents & libc::POLLIN != 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        if queue.dispatch_pending(&mut st).is_err() {
            break;
        }
        if last.elapsed() >= Duration::from_secs(2) {
            last = Instant::now();
            if st.ed.refresh(read_live(), read_dns()) {
                st.render();
            }
        }
    }
    if let Some(l) = st.layer.take() {
        l.destroy();
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

    fn ed() -> Editor {
        let live = vec![
            Live { name: "eth0".into(), state: "up".into(), ..Live::default() },
            Live { name: "eth1".into(), state: "down".into(), ..Live::default() },
        ];
        Editor::new(Vec::new(), live, Vec::new())
    }

    fn typed(e: &mut Editor, s: &str) {
        for c in s.chars() {
            e.type_char(c);
        }
    }

    #[test]
    fn the_panel_and_its_fields_fit_every_output() {
        for (w, h, n) in [(1920, 1080, 2), (1280, 720, 1), (1024, 768, 6), (800, 600, 0)] {
            let l = layout(w, h, n);
            let (px, py, pw, ph) = l.panel;
            assert!(px >= 0 && py >= 0 && px + pw <= w, "{w}x{h}");
            assert!(h < 720 || py + ph <= h, "{w}x{h}: {ph}");
            for r in [l.addr4, l.gw4, l.addr6, l.gw6, l.dns, l.revert, l.apply]
                .into_iter()
                .chain(l.seg4)
                .chain(l.seg6)
            {
                assert!(r.0 >= px && r.0 + r.2 <= px + pw, "{w}x{h}: {r:?}");
                assert!(r.1 >= py && r.1 + r.3 <= py + ph, "{w}x{h}: {r:?}");
            }
            // Nothing overlaps the mode buttons.
            assert!(l.seg4[2].1 + FIELD_H <= l.addr4.1);
            assert!(l.gw4.1 + FIELD_H <= l.sec6);
        }
    }

    #[test]
    fn tab_skips_the_addresses_dhcp_gives() {
        let mut e = ed();
        let i = e.edit[0].clone();
        assert_eq!(step(Focus::Mode4, &i, false), Focus::Mode6);
        assert_eq!(step(Focus::Mode6, &i, false), Focus::Dns);
        assert_eq!(step(Focus::List, &i, true), Focus::Apply);
        e.focus = Focus::Mode4;
        e.key(KEY_RIGHT_WL, false);
        assert_eq!(e.edit[0].v4, V4::Static);
        e.key(KEY_TAB_WL, false);
        assert_eq!(e.focus, Focus::Addr4);
        e.key(KEY_TAB_WL, true);
        assert_eq!(e.focus, Focus::Mode4);
        e.key(KEY_LEFT_WL, false);
        e.key(KEY_LEFT_WL, false);
        assert_eq!(e.edit[0].v4, V4::Off, "← wraps from DHCP to APAGADO");
    }

    #[test]
    fn the_keys_type_an_address_on_either_layout() {
        // 192.168.1.20/24 on a Spanish keyboard: `/` is Shift+7.
        let mut e = ed();
        e.edit[0].v4 = V4::Static;
        e.focus = Focus::Addr4;
        for (k, sh) in [(2, false), (10, false), (3, false), (52, false), (2, false), (7, false), (9, false)] {
            e.key(k, sh);
        }
        e.key(52, false);
        e.key(2, false);
        e.key(52, false);
        e.key(3, false);
        e.key(11, false);
        e.key(8, true);
        e.key(3, false);
        e.key(5, false);
        assert_eq!(e.edit[0].address4, "192.168.1.20/24");
        e.key(KEY_BACKSPACE_WL, false);
        assert_eq!(e.edit[0].address4, "192.168.1.20/2");
        // `:` is Shift+. (ES) or Shift+; (US); the hex letters type, others not.
        assert_eq!(field_char(52, true), Some(':'));
        assert_eq!(field_char(39, true), Some(':'));
        assert_eq!(field_char(33, false), Some('f'));
        assert_eq!(field_char(34, false), None, "g is not hex");
        assert_eq!(field_char(80, false), Some('2'), "keypad");
        // A field DHCP fills cannot be typed into.
        let mut e = ed();
        e.focus = Focus::Addr4;
        e.type_char('1');
        assert_eq!(e.edit[0].address4, "");
    }

    #[test]
    fn apply_refuses_a_bad_field_and_points_at_it() {
        let mut e = ed();
        e.select(1);
        e.edit[1].v6 = V6::Static;
        e.focus = Focus::Addr6;
        typed(&mut e, "2001:db8::zz");
        e.select(0);
        let mut saved = false;
        e.apply(
            |_| {
                saved = true;
                Ok(())
            },
            || true,
        );
        assert!(!saved);
        assert_eq!((e.sel, e.focus, e.tone), (1, Focus::Addr6, Tone::Bad));
        assert!(e.status.starts_with("eth1: "), "{}", e.status);
    }

    #[test]
    fn apply_saves_only_what_is_not_the_default_and_says_what_happened() {
        let mut e = ed();
        e.edit[1].v4 = V4::Static;
        e.edit[1].address4 = "192.168.7.2/24".into();
        e.edit[1].gateway4 = "192.168.7.1".into();
        assert_eq!(e.dirty(), vec![false, true]);
        let mut got = Vec::new();
        e.apply(
            |a| {
                got = a.to_vec();
                Ok(())
            },
            || false,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "eth1");
        assert_eq!(e.dirty(), vec![false, false]);
        assert_eq!(e.tone, Tone::Bad, "saved, but no service to tell");
        e.edit[1].dns = "9.9.9.9".into();
        e.apply(|_| Ok(()), || true);
        assert_eq!(
            (e.status.as_str(), e.tone),
            (e.lang.s(Key::net_saved_applied), Tone::Good)
        );
        e.apply(|_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)), || true);
        assert_eq!(e.tone, Tone::Bad);
        // Deshacer goes back to the file.
        e.edit[1].v4 = V4::Off;
        e.select(1);
        e.revert();
        assert_eq!(e.edit[1].v4, V4::Static);
    }

    #[test]
    fn a_click_picks_the_interface_the_mode_and_the_field() {
        let (w, h) = (1600, 900);
        let mut e = ed();
        let l = layout(w, h, 2);
        let mid = |r: Rect| (r.0 + r.2 / 2, r.1 + r.3 / 2);
        let (x, y) = mid(iface_row(&l, 1));
        assert!(e.click(w, h, x, y));
        assert_eq!(e.sel, 1);
        let (x, y) = mid(l.seg6[2]);
        e.click(w, h, x, y);
        assert_eq!((e.edit[1].v6, e.focus), (V6::Static, Focus::Mode6));
        let (x, y) = mid(l.gw6);
        e.click(w, h, x, y);
        assert_eq!(e.focus, Focus::Gw6);
        // A click outside keeps the window while something is unsaved…
        assert!(e.click(w, h, 2, 2));
        e.revert();
        // …and closes it when nothing is.
        assert!(!e.click(w, h, 2, 2));
    }

    #[test]
    fn a_fresh_look_keeps_what_is_being_typed() {
        let mut e = ed();
        e.select(1);
        e.edit[1].v4 = V4::Off;
        let live = vec![
            Live { name: "eth1".into(), state: "up".into(), ..Live::default() },
            Live { name: "wlan0".into(), ..Live::default() },
        ];
        assert!(e.refresh(live, vec!["10.0.0.1".into()]));
        assert_eq!(e.edit.len(), 2);
        assert_eq!((e.sel, e.edit[0].v4), (0, V4::Off), "eth1 is still selected and edited");
        assert_eq!(e.edit[1].v4, V4::Dhcp);
    }

    #[test]
    fn every_size_draws_without_a_panic() {
        let s = sample();
        for (w, h) in [(1600, 900), (1024, 600), (640, 480), (320, 240)] {
            s.draw(&mut Canvas::new(w, h), w as i32, h as i32);
        }
        let empty = Editor::new(Vec::new(), Vec::new(), Vec::new());
        empty.draw(&mut Canvas::new(800, 600), 800, 600);
        let mut e = Editor::new(Vec::new(), Vec::new(), Vec::new());
        assert!(e.key(KEY_TAB_WL, false));
        assert!(!e.key(KEY_ESC_WL, false));
    }
}
