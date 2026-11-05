//! The top bar ("info" role): launcher wordmark, system readouts, date and
//! power. Its own file because `main.rs` is at its size ceiling (see
//! `xtask/src/tamano.rs`); a child module of the binary so it shares the
//! palette and helpers. The `tecno` look draws mockup A's layout instead, see
//! [`draw_info_tecno`].

use super::*;

/// Cells in a segmented meter, and the width both meters occupy. Seven cells
/// of 3px with a 1px gap come to 27, one over the continuous gauge's 26, so
/// the gauge is the one that gives: the modules are laid out from the right
/// edge and a meter that measured differently per look would move everything
/// to its left.
pub(crate) const METER_CELLS: i32 = 7;
pub(crate) const METER_W: i32 = METER_CELLS * 4 - 1;
/// Height of a module's history plot. Short enough to sit inside the row of
/// text it belongs to, tall enough that a spike is a spike.
pub(crate) const HIST_H: i32 = 11;

/// Width a module has to reserve for its history, including the gap before
/// the label — zero in the looks that do not draw one, and zero until there
/// is something to draw.
pub(crate) fn hist_w(r: &meter::Ring) -> i32 {
    if tecno() && !r.is_empty() {
        METER_W + 6
    } else {
        0
    }
}

/// Draw that history. The slot is exactly as wide as the segmented meter, so
/// a module that gains a past does not move the modules to its left.
pub(crate) fn hist(cv: &mut Canvas, x: i32, h: i32, r: &meter::Ring, scaled: bool) -> i32 {
    let w = hist_w(r);
    if w == 0 {
        return 0;
    }
    let p = pal();
    meter::plot(cv, x, (h - HIST_H) / 2, METER_W, HIST_H, r, scaled, p.pill, p.accent, p.warn);
    w
}

pub(crate) fn metric(
    cv: &mut Canvas,
    right: i32,
    min_x: i32,
    ty: i32,
    h: i32,
    label: &str,
    gauge: Option<f32>,
    col: Rgb,
) -> Option<i32> {
    let tw = Canvas::text_width(label);
    // The two meters must measure the SAME, or switching looks would shift
    // every module on the bar by a few pixels and the hit boxes with them.
    let (gw, gpad) = if gauge.is_some() { (METER_W, 6) } else { (0, 0) };
    let total = gw + gpad + tw;
    let x = right - total;
    if x < min_x {
        return None;
    }
    if let Some(f) = gauge {
        let ghh = 7;
        let gy = (h - ghh) / 2;
        if tecno() {
            // Segments, not a continuous bar: a meter with a scale on it is
            // the one piece of this look that is not just a colour change.
            // And the lit colour is the sample's, not the theme's: one
            // ramp, no thresholds to argue about.
            let c = meter::ramp(f, pal().accent, pal().warn);
            cv.blocks(x, gy, METER_CELLS, f, ghh, c, pal().dim);
        } else {
            cv.gauge(x, gy, gw, ghh, f, pal().pill);
        }
    }
    cv.text(label, x + gw + gpad, ty, col);
    Some(x)
}

pub(crate) fn net_module(
    cv: &mut Canvas,
    right: i32,
    min_x: i32,
    ty: i32,
    h: i32,
    n: &NetRate,
    hd: &meter::Ring,
) -> Option<i32> {
    let down = sysinfo::fmt_rate(n.down);
    let up = sysinfo::fmt_rate(n.up);
    let ts = 9;
    let ty_tri = (h - ts) / 2;
    let dw = Canvas::text_width(&down);
    let uw = Canvas::text_width(&up);
    let total = hist_w(hd) + ts + 4 + dw + 10 + ts + 4 + uw;
    let x = right - total;
    if x < min_x {
        return None;
    }
    let mut cx = x;
    // Scaled to its own window: a link that idles at 2 kB/s and bursts to 2
    // MB/s has no useful fixed axis, so the plot picks one every tick.
    cx += hist(cv, cx, h, hd, true);
    cv.triangle(cx, ty_tri, ts, false, pal().accent);
    cx += ts + 4;
    cx += cv.text(&down, cx, ty, pal().muted);
    cx += 10;
    cv.triangle(cx, ty_tri, ts, true, pal().accent);
    cx += ts + 4;
    cv.text(&up, cx, ty, pal().muted);
    Some(x)
}

pub(crate) fn draw_info(cv: &mut Canvas, w: usize, h: usize, m: &Metrics, hover: Hover) -> ((i32, i32), (i32, i32), (i32, i32)) {
    if tecno() {
        return draw_info_tecno(cv, w, h, m, hover);
    }
    clear_bar(cv, w, h);
    bar_edge(cv, w as i32, h as i32 - 2, h as i32 - 1, true);

    let ty = (h as i32 - 2 - GLYPH_H) / 2;
    let hi = h as i32;
    let btn_h = hi - 10;
    let btn_y = (hi - btn_h) / 2;

    let d = (hi * 18) / 34;
    let ly = (hi - d) / 2;
    let mut lx = 10 + d + 8;
    let wordmark = cap("eclipse");
    let wordmark_w = Canvas::text_width(&wordmark);
    let launcher_hit = (0, lx + wordmark_w + 4);
    if hover == Hover::Launcher {
        plate_a(cv, 2, btn_y, launcher_hit.1 - 4, btn_h, 6, pal().menu_hover, 0.45);
    }
    cv.crescent(10, ly, d, pal().accent);
    lx += cv.text_bold(&wordmark, lx, ty, if tecno() { pal().accent } else { pal().text });

    if let Some(up) = &m.uptime {
        lx += 12;
        cv.vrule(lx, hi, pal().rule);
        lx += 12;
        lx += cv.text(&cap(&format!("up {up}")), lx, ty, pal().muted);
    }
    if let Some(load) = m.load {
        lx += 12;
        cv.vrule(lx, hi, pal().rule);
        lx += 12;
        lx += cv.text(&cap(&format!("load {load:.2}")), lx, ty, pal().muted);
    }

    let min_x = lx + 12;
    let mut rx = w as i32 - 4;
    let mut clock_hit = (0, 0);
    let mut power_hit = (0, 0);

    {
        let pw_size = 14;
        let pw_w = pw_size + 16;
        if rx - pw_w >= min_x {
            rx -= pw_w;
            let pill = if hover == Hover::Power { pal().pill_hover } else { pal().pill };
            plate(cv, rx, btn_y, pw_w, btn_h, 6, pill);
            cv.power_icon(rx + (pw_w - pw_size) / 2, btn_y + (btn_h - pw_size) / 2, pw_size, if tecno() { pal().warn } else { pal().white });
            power_hit = (rx, rx + pw_w);
            rx -= 10;
        }

        let pw = Canvas::text_width(&cap(&m.date)) + 20;
        if rx - pw >= min_x {
            rx -= pw;
            let pill = if hover == Hover::Clock { pal().pill_hover } else { pal().pill };
            plate(cv, rx, btn_y, pw, btn_h, 6, pill);
            cv.text_bold(&cap(&m.date), rx + 10, ty, pal().text);
            clock_hit = (rx, rx + pw);
            rx -= 10;
        }
    }
    if let Some((b, ch)) = m.batt {
        let label = cap(&if ch { format!("bat {b}% +") } else { format!("bat {b}%") });
        let col = if b < 15 && !ch { pal().warn } else { pal().muted };
        if let Some(x) = metric(cv, rx - 10, min_x, ty, hi, &label, Some(b as f32 / 100.0), col) {
            rx = x - 12;
            cv.vrule(rx, hi, pal().rule);
        }
    }
    if let Some(t) = m.temp {
        let col = if t >= 85 { pal().warn } else { pal().muted };
        if let Some(x) = metric(cv, rx - 10, min_x, ty, hi, &cap(&format!("{t}°c")), None, col) {
            rx = x - 12;
            cv.vrule(rx, hi, pal().rule);
        }
    }
    if let Some(dk) = m.disk {
        if let Some(x) = metric(
            cv,
            rx - 10,
            min_x,
            ty,
            hi,
            &cap(&format!("disk {dk}%")),
            Some(dk as f32 / 100.0),
            pal().muted,
        ) {
            rx = x - 12;
            cv.vrule(rx, hi, pal().rule);
        }
    }
    if let Some(n) = &m.net {
        if n.link {
            net_module(cv, rx - 10, min_x, ty, hi, n, &m.net_hist);
        }
    }

    (launcher_hit, clock_hit, power_hit)
}

/// Mockup A's top bar: wordmark on the left, the focused window centred, and
/// on the right the instrument row `CPU ▮▮▮▯ 38%  MEM ▮▮▯ 41%  NET ▼ ▲
/// TEMP 54°` with the date chip and power after it. Uptime, load and disk
/// stay in the other looks; A has no room for them and they rarely move.
/// Modules are laid out from the right and dropped whole when they do not
/// fit, as `draw_info` does, and the title only gets what is left.
pub(crate) fn draw_info_tecno(
    cv: &mut Canvas,
    w: usize,
    h: usize,
    m: &Metrics,
    hover: Hover,
) -> ((i32, i32), (i32, i32), (i32, i32)) {
    clear_bar(cv, w, h);
    bar_edge(cv, w as i32, h as i32 - 2, h as i32 - 1, true);
    let p = pal();
    let hi = h as i32;
    let ty = (hi - 2 - GLYPH_H) / 2;
    let btn_h = hi - 10;
    let btn_y = (hi - btn_h) / 2;

    // Wordmark, letter-spaced like the mockup's.
    let d = (hi * 18) / 34;
    cv.crescent(10, (hi - d) / 2, d, p.accent);
    let wordmark = "E C L I P S E";
    let lx = 10 + d + 8;
    let wordmark_w = Canvas::text_width(wordmark);
    let launcher_hit = (0, lx + wordmark_w + 4);
    if hover == Hover::Launcher {
        plate(cv, 2, btn_y, launcher_hit.1 - 4, btn_h, 6, p.menu_hover);
        cv.crescent(10, (hi - d) / 2, d, p.accent);
    }
    cv.text_bold(wordmark, lx, ty, p.accent);
    let min_x = launcher_hit.1 + 16;

    let mut rx = w as i32 - 4;
    let (mut clock_hit, mut power_hit) = ((0, 0), (0, 0));
    let pw_w = 14 + 16;
    if rx - pw_w >= min_x {
        rx -= pw_w;
        plate(cv, rx, btn_y, pw_w, btn_h, 6, if hover == Hover::Power { p.pill_hover } else { p.pill });
        cv.power_icon(rx + 8, btn_y + (btn_h - 14) / 2, 14, p.warn);
        power_hit = (rx, rx + pw_w);
        rx -= 10;
    }
    let date = cap(&m.date);
    let dw = Canvas::text_width(&date) + 20;
    if rx - dw >= min_x {
        rx -= dw;
        plate(cv, rx, btn_y, dw, btn_h, 6, if hover == Hover::Clock { p.pill_hover } else { p.pill });
        cv.text_bold(&date, rx + 10, ty, p.text);
        clock_hit = (rx, rx + dw);
        rx -= 18;
    }

    // Right to left: TEMP, NET, MEM, CPU, then the battery if there is one.
    let hot = |v: Option<u32>, at: u32| if v.unwrap_or(0) >= at { p.warn } else { p.text };
    if let Some(t) = m.temp {
        let col = if t >= 85 { p.warn } else { TECNO_GOLD };
        rx = readout(cv, rx, min_x, ty, hi, "TEMP", None, &format!("{t}°"), col).unwrap_or(rx);
    }
    if let Some(n) = m.net.as_ref().filter(|n| n.link) {
        let lw = Canvas::text_width("NET") + 8;
        if let Some(x) = net_module(cv, rx, min_x + lw, ty, hi, n, &meter::Ring::default()) {
            cv.text("NET", x - lw, ty, p.dim);
            rx = x - lw - 18;
        }
    }
    let mem = m.mem.map(|v| v as f32 / 100.0);
    rx = readout(cv, rx, min_x, ty, hi, "MEM", mem, &format!("{}%", opt(m.mem)), hot(m.mem, 90)).unwrap_or(rx);
    let cpu = m.cpu.map(|v| v as f32 / 100.0);
    rx = readout(cv, rx, min_x, ty, hi, "CPU", cpu, &format!("{}%", opt(m.cpu)), hot(m.cpu, 90)).unwrap_or(rx);
    if let Some((b, ch)) = m.batt {
        let v = if ch { format!("{b}% +") } else { format!("{b}%") };
        let col = if b < 15 && !ch { p.warn } else { p.text };
        rx = readout(cv, rx, min_x, ty, hi, "BAT", Some(b as f32 / 100.0), &v, col).unwrap_or(rx);
    }

    // The focused window, centred on the SCREEN and cut to the room left.
    let room = (rx + 2 - min_x).max(0);
    let max_chars = (room / GLYPH_W) as usize;
    let mut title: String = m.title.chars().take(max_chars).collect();
    if m.title.chars().count() > max_chars && max_chars > 1 {
        title.pop();
        title.push('.');
    }
    if !title.is_empty() && max_chars > 3 {
        let tw = Canvas::text_width(&title);
        let tx = ((w as i32 - tw) / 2).clamp(min_x, (rx + 2 - tw).max(min_x));
        cv.text(&title, tx, ty, p.dim);
    }

    (launcher_hit, clock_hit, power_hit)
}

/// `LABEL [meter] value`, right-aligned at `right`; the new right edge (with
/// the gap to the next module) or None if it did not fit.
#[allow(clippy::too_many_arguments)]
fn readout(
    cv: &mut Canvas,
    right: i32,
    min_x: i32,
    ty: i32,
    h: i32,
    label: &str,
    gauge: Option<f32>,
    value: &str,
    col: Rgb,
) -> Option<i32> {
    let (lw, vw) = (Canvas::text_width(label), Canvas::text_width(value));
    let gw = if gauge.is_some() { METER_W + 8 } else { 0 };
    let x = right - (lw + 8 + gw + vw);
    if x < min_x {
        return None;
    }
    cv.text(label, x, ty, pal().dim);
    if let Some(f) = gauge {
        let c = meter::ramp(f, pal().accent, pal().warn);
        cv.blocks(x + lw + 8, (h - 9) / 2, METER_CELLS, f, 9, c, pal().dim);
    }
    cv.text(value, x + lw + 8 + gw, ty, col);
    Some(x - 18)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(title: &str, w: usize) -> Vec<u8> {
        let h = 34;
        let m = Metrics {
            cpu: Some(38),
            mem: Some(41),
            date: "jue 05 nov".into(),
            temp: Some(54),
            title: title.into(),
            ..Default::default()
        };
        let mut cv = Canvas::new(w, h);
        draw_info_tecno(&mut cv, w, h, &m, Hover::None);
        let mut out = vec![0u8; w * h * 4];
        blit_preview(&cv, &mut out, w, h);
        out
    }

    fn column_differs(a: &[u8], b: &[u8], w: usize, x: usize) -> bool {
        (0..a.len() / (w * 4)).any(|y| {
            let i = (y * w + x) * 4;
            a[i..i + 3] != b[i..i + 3]
        })
    }

    #[test]
    fn a_long_title_is_cut_before_it_reaches_the_readouts() {
        for w in [1440usize, 1024, 800] {
            let none = top("", w);
            let long = top(&"x".repeat(400), w);
            // Leftmost column the readouts draw in: the first one past the
            // wordmark that differs from a bar with nothing on the right.
            let bare = {
                let mut cv = Canvas::new(w, 34);
                draw_info_tecno(&mut cv, w, 34, &Metrics::default(), Hover::None);
                let mut out = vec![0u8; w * 34 * 4];
                blit_preview(&cv, &mut out, w, 34);
                out
            };
            let first = (200..w).find(|&x| column_differs(&none, &bare, w, x)).expect("no readouts");
            let last_title = (0..w).rev().find(|&x| column_differs(&none, &long, w, x)).expect("no title");
            assert!(last_title + 8 < first, "{w}: title reaches x={last_title}, readouts start at {first}");
        }
    }

    #[test]
    fn the_title_is_the_focused_window_as_app_and_title() {
        // active_title is this on the activated toplevel; handles need a
        // live compositor, so the rule is checked on title_of itself.
        assert_eq!(title_of("foot", "root@eclipse: ~"), "foot · root@eclipse: ~");
        assert_eq!(title_of("foot", ""), "foot");
        assert_eq!(title_of("", "Notas — proyecto"), "Notas - proyecto");
        assert_eq!(title_of("Firefox", "firefox"), "firefox");
    }
}
