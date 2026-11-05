//! The volume control: what the bar's volume module opens.
//!
//! It was a meter and a number, set by clicking somewhere on the meter. Now
//! it is a complete volume control: the level and its colour in the header,
//! the name of the device the sound is going to, a mute switch, − and + steps either side of the meter, the usual levels
//! (0, 25, 50, 75, 100) one click away under it, ←/→ (or ↑/↓, or the wheel)
//! to step and M to mute. The level and the mute survive the session: each
//! change is written to `$XDG_STATE_HOME/lunar/volume` (default
//! `~/.local/state/lunar/volume`), and the next session puts them back. The meter's colour climbs from the accent to the
//! warning as the level does, and the whole control goes dim when muted.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;

const PW: i32 = 272;
const HEAD_H: i32 = 36;
/// The row naming the output device(s).
const DEV_H: i32 = 24;
const METER_H: i32 = 28;
const TICK_H: i32 = 20;
const PAD: i32 = 8;
/// The − and + buttons, and the mute switch.
const BTN: i32 = 20;
/// One step of − / +, the arrows and the wheel.
pub const STEP: i32 = 5;
/// The levels under the meter.
pub const PRESETS: [u32; 5] = [0, 25, 50, 75, 100];

pub fn size() -> (i32, i32) {
    (PW, HEAD_H + DEV_H + METER_H + TICK_H + PAD)
}

/// The meter's x and width inside a panel at `px` `pw` wide.
pub fn meter(px: i32, pw: i32) -> (i32, i32) {
    let x = px + 12 + BTN + 8;
    (x, (px + pw - 12 - BTN - 8) - x)
}

/// The level a click at `x` on the meter asks for.
pub fn level_at(px: i32, pw: i32, x: i32) -> u32 {
    let (mx, mw) = meter(px, pw);
    let rel = (x - mx).clamp(0, mw.max(1));
    ((rel as u32 * 100 + mw.max(1) as u32 / 2) / mw.max(1) as u32).min(100)
}

/// `v` stepped by `d`, kept in 0..=100.
pub fn step(v: u32, d: i32) -> u32 {
    (v as i32 + d).clamp(0, 100) as u32
}

/// The level's colour, in mockup A's steps rather than a blend (a blend of
/// cyan and orange passes through grey): the accent for a normal level, the
/// gold of a caution from 70 %, the warning from 90 %; dim when muted.
fn colour(v: u32, muted: bool) -> Rgb {
    match (muted, v) {
        (true, _) => pal().dim,
        (_, 90..) => pal().warn,
        (_, 70..) => TECNO_GOLD,
        _ => pal().accent,
    }
}

/// Whether the default sink is muted, by whichever tool the machine has.
pub fn muted() -> bool {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };
    if let Some(s) = run("pactl", &["get-sink-mute", "@DEFAULT_SINK@"]) {
        return s.contains("yes");
    }
    if let Some(s) = run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]) {
        return s.contains("MUTED");
    }
    run("amixer", &["get", "Master"]).is_some_and(|s| s.contains("[off]"))
}

/// The output device(s) in use, by name, as the sound server describes
/// them: PipeWire/PulseAudio's default sink, else ALSA's cards. Empty when
/// the machine has none it can name.
pub fn devices() -> Vec<String> {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };
    if let (Some(def), Some(list)) = (run("pactl", &["get-default-sink"]), run("pactl", &["list", "sinks"])) {
        if let Some(d) = sink_description(&list, def.trim()) {
            return vec![d];
        }
    }
    if let Some(s) = run("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"]) {
        if let Some(d) = prop(&s, "node.description").or_else(|| prop(&s, "node.nick")) {
            return vec![d];
        }
    }
    std::fs::read_to_string("/proc/asound/cards").map(|s| alsa_cards(&s)).unwrap_or_default()
}

/// The `Description:` of sink `name` in `pactl list sinks` output.
fn sink_description(list: &str, name: &str) -> Option<String> {
    let mut this = false;
    for line in list.lines() {
        let l = line.trim();
        if let Some(n) = l.strip_prefix("Name:") {
            this = n.trim() == name;
        } else if this {
            if let Some(d) = l.strip_prefix("Description:") {
                return Some(d.trim().to_string()).filter(|d| !d.is_empty());
            }
        }
    }
    None
}

/// A `key = "value"` property from `wpctl inspect`.
fn prop(s: &str, key: &str) -> Option<String> {
    s.lines().find_map(|l| {
        let l = l.trim().trim_start_matches('*').trim();
        let v = l.strip_prefix(key)?.trim().strip_prefix('=')?.trim();
        Some(v.trim_matches('"').to_string()).filter(|v| !v.is_empty())
    })
}

/// Card names from `/proc/asound/cards` (` 0 [PCH ]: HDA-Intel - HDA Intel PCH`).
fn alsa_cards(s: &str) -> Vec<String> {
    s.lines()
        .filter(|l| l.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()))
        .filter_map(|l| l.split_once(" - ").map(|(_, n)| n.trim().to_string()))
        .filter(|n| !n.is_empty())
        .collect()
}

/// Paint the control and return its hitboxes.
pub fn draw(
    cv: &mut Canvas,
    px: i32,
    py: i32,
    pw: i32,
    v: u32,
    muted: bool,
    devices: &[String],
) -> Vec<(i32, i32, i32, i32, Action)> {
    draw_panel(cv, px, py, pw, size().1, 10);
    let c = colour(v, muted);
    let mut hits = Vec::new();

    // Header: speaker, level, mute switch.
    let es = i18n::Lang::current() == i18n::Lang::Es;
    cv.volume_icon(px + 14, py + (HEAD_H - 16) / 2, 16, c, muted || v == 0);
    let label = if muted {
        cap(if es { "silencio" } else { "muted" })
    } else {
        cap(&format!("{} {v}%", if es { "volumen" } else { "volume" }))
    };
    cv.text_bold(&label, px + 40, py + (HEAD_H - GLYPH_H) / 2, if muted { pal().dim } else { pal().text });
    let (bx, by) = (px + pw - 12 - BTN, py + (HEAD_H - BTN) / 2);
    let edge = if muted { pal().warn } else { pal().rule };
    cv.glyph_box(bx, by, BTN, "", pal().pill, edge, pal().accent);
    cv.volume_icon(bx + 3, by + 3, BTN - 6, if muted { pal().warn } else { pal().dim }, true);
    hits.push((bx, by, bx + BTN, by + BTN, Action::VolumeMute));
    cv.hline(px + 10, py + HEAD_H - 1, pw - 20, pal().rule, 0.7);

    // Where the sound is going: the device(s) by name, cut to the panel.
    let es_none = if es { "sin dispositivo" } else { "no device" };
    let names = if devices.is_empty() { es_none.to_string() } else { devices.join(" + ") };
    let max = ((pw - 28) / GLYPH_W) as usize;
    let names: String = if names.chars().count() > max {
        names.chars().take(max.saturating_sub(1)).chain(['.']).collect()
    } else {
        names
    };
    let dy = py + HEAD_H + (DEV_H - GLYPH_H) / 2;
    cv.text(&names, px + 14, dy, pal().muted);

    // − meter +
    let top = py + HEAD_H + DEV_H;
    let my = top + (METER_H - BTN) / 2;
    let (mx, mw) = meter(px, pw);
    for (x, d, ch) in [(px + 12, -STEP, "-"), (px + pw - 12 - BTN, STEP, "+")] {
        cv.glyph_box(x, my, BTN, ch, pal().pill, pal().rule, c);
        hits.push((x, my, x + BTN, my + BTN, Action::VolumeStep(d as i8)));
    }
    let gy = top + (METER_H - 10) / 2;
    let frac = if muted { 0.0 } else { v as f32 / 100.0 };
    if tecno() {
        cv.blocks(mx, gy, (mw + 1) / 4, frac, 10, c, pal().dim);
    } else {
        cv.gauge(mx, gy, mw, 10, frac, pal().pill);
    }
    hits.push((mx, gy - 8, mx + mw, gy + 18, Action::VolumeGauge));

    // The usual levels, under the points of the meter they set.
    let ty = top + METER_H;
    for p in PRESETS {
        let s = p.to_string();
        let cx = mx + (mw * p as i32) / 100;
        let tw = Canvas::text_width(&s);
        let tx = (cx - tw / 2).clamp(px + 8, px + pw - 8 - tw);
        let on = !muted && p == v;
        cv.text(&s, tx, ty + (TICK_H - GLYPH_H) / 2, if on { c } else { pal().dim });
        hits.push((tx - 2, ty, tx + tw + 2, ty + TICK_H, Action::VolumePreset(p as u8)));
    }
    hits
}

/// Where the last level and mute are kept between sessions.
fn saved_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.local/state")))?;
    Some(std::path::Path::new(&base).join("lunar").join("volume"))
}

/// The saved file's text for level `v` and `muted`.
fn encode(v: u32, muted: bool) -> String {
    format!("level={v}\nmuted={}\n", u8::from(muted))
}

/// The level and mute in a saved file; `None` when it says nothing usable.
fn decode(s: &str) -> Option<(u32, bool)> {
    let mut level = None;
    let mut muted = false;
    for l in s.lines() {
        match l.split_once('=') {
            Some(("level", v)) => level = v.trim().parse::<u32>().ok().filter(|v| *v <= 100),
            Some(("muted", v)) => muted = v.trim() == "1",
            _ => {}
        }
    }
    level.map(|v| (v, muted))
}

/// Remember `v` and `muted` for the next session.
fn save(v: u32, muted: bool) {
    if let Some(p) = saved_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, encode(v, muted));
    }
}

/// What the last session left, if anything.
pub fn saved() -> Option<(u32, bool)> {
    decode(&std::fs::read_to_string(saved_path()?).ok()?)
}

/// The commands that set level `v` and mute `on` on whichever tool exists.
fn set_cmd(v: u32) -> String {
    format!(
        "pactl set-sink-volume @DEFAULT_SINK@ {v}% >/dev/null 2>&1 || amixer set Master {v}% >/dev/null 2>&1 || wpctl set-volume @DEFAULT_AUDIO_SINK@ {v}%"
    )
}

fn mute_cmd(on: bool) -> String {
    let (p, a, w) = if on { ("1", "mute", "1") } else { ("0", "unmute", "0") };
    format!(
        "pactl set-sink-mute @DEFAULT_SINK@ {p} >/dev/null 2>&1 || amixer set Master {a} >/dev/null 2>&1 || wpctl set-mute @DEFAULT_AUDIO_SINK@ {w}"
    )
}

impl State {
    /// At start: put back the level and mute the last session left, so the
    /// volume is where the user set it, not wherever the card woke up.
    pub(crate) fn restore_volume(&mut self) {
        let Some((v, m)) = saved() else {
            return;
        };
        self.vol = Some(v);
        self.vol_muted = m;
        self.spawn(&format!("{} ; {}", set_cmd(v), mute_cmd(m)));
    }

    /// Set the volume to `v` (unmuting it), on the machine and on screen.
    pub(crate) fn set_volume(&mut self, qh: &QueueHandle<State>, v: u32) {
        if self.vol_muted {
            self.set_mute(qh, false);
        }
        self.vol = Some(v);
        if let Some(Popup { kind: PopupKind::Volume { level, .. }, .. }) = self.popup.as_mut() {
            *level = v;
        }
        self.spawn(&set_cmd(v));
        save(v, self.vol_muted);
        self.render_popup(qh);
        self.render_all();
    }

    /// Mute or unmute the default sink.
    pub(crate) fn set_mute(&mut self, qh: &QueueHandle<State>, on: bool) {
        self.vol_muted = on;
        if let Some(Popup { kind: PopupKind::Volume { muted, .. }, .. }) = self.popup.as_mut() {
            *muted = on;
        }
        self.spawn(&mute_cmd(on));
        save(self.vol.unwrap_or(0), on);
        self.render_popup(qh);
        self.render_all();
    }

    /// A press on the control: `x` is where, for the meter.
    pub(crate) fn volume_act(&mut self, qh: &QueueHandle<State>, a: Action, x: i32) {
        let (px, pw) = self.popup.as_ref().map_or((0, 1), |p| (p.panel.0, p.panel.2));
        let v = self.vol.unwrap_or(0);
        match a {
            Action::VolumeGauge => self.set_volume(qh, level_at(px, pw, x)),
            Action::VolumeStep(d) => self.set_volume(qh, step(v, d as i32)),
            Action::VolumePreset(p) => self.set_volume(qh, p as u32),
            Action::VolumeMute => self.set_mute(qh, !self.vol_muted),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_reads_its_ends_and_its_middle() {
        let (px, pw) = (12, PW);
        let (mx, mw) = meter(px, pw);
        assert_eq!(level_at(px, pw, mx - 50), 0);
        assert_eq!(level_at(px, pw, mx), 0);
        assert_eq!(level_at(px, pw, mx + mw / 2), 50);
        assert_eq!(level_at(px, pw, mx + mw), 100);
        assert_eq!(level_at(px, pw, mx + mw + 50), 100);
    }

    #[test]
    fn the_device_is_named_by_whichever_tool_answers() {
        let list = "Sink #0\n\tName: alsa_output.a\n\tDescription: Altavoces\nSink #1\n\tName: bluez.b\n\tDescription: Auriculares BT\n";
        assert_eq!(sink_description(list, "bluez.b").as_deref(), Some("Auriculares BT"));
        assert_eq!(sink_description(list, "nope"), None);
        let w = "id 52, type PipeWire:Interface:Node\n  * node.description = \"Built-in Audio Analog Stereo\"\n    node.name = \"alsa\"\n";
        assert_eq!(prop(w, "node.description").as_deref(), Some("Built-in Audio Analog Stereo"));
        let cards = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0xf7f10000 irq 32\n 1 [Device         ]: USB-Audio - USB Audio Device\n";
        assert_eq!(alsa_cards(cards), ["HDA Intel PCH", "USB Audio Device"]);
    }

    #[test]
    fn the_saved_volume_reads_back_and_refuses_nonsense() {
        assert_eq!(decode(&encode(42, true)), Some((42, true)));
        assert_eq!(decode(&encode(0, false)), Some((0, false)));
        assert_eq!(decode("level=250\n"), None);
        assert_eq!(decode("garbage"), None);
        assert_eq!(decode("muted=1\nlevel=7\n"), Some((7, true)));
    }

    #[test]
    fn steps_stay_in_range() {
        assert_eq!(step(3, -STEP), 0);
        assert_eq!(step(98, STEP), 100);
        assert_eq!(step(40, STEP), 45);
    }

    #[test]
    fn every_hitbox_is_inside_the_panel() {
        let (px, py) = (POPUP_SHADOW, POPUP_SHADOW);
        let (pw, ph) = size();
        let mut cv = Canvas::try_new((pw + 2 * px) as usize, (ph + 2 * py) as usize).unwrap();
        for (x0, y0, x1, y1, a) in draw(&mut cv, px, py, pw, 42, false, &["HDA Intel PCH".into()]) {
            assert!(x0 >= px && y0 >= py && x1 <= px + pw && y1 <= py + ph, "{x0},{y0}..{x1},{y1}");
            let _ = a;
        }
    }
}
