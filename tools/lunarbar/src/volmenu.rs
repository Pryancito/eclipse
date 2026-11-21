//! The volume control: what the bar's volume module opens.
//!
//! It was a meter and a number, set by clicking somewhere on the meter. Now
//! it is a complete volume control: the level and its colour in the header,
//! the outputs the sound can go to (one row each, the one in use marked; a
//! click on another moves the sound there), a mute switch, − and + steps either side of the meter, the usual levels
//! (0, 25, 50, 75, 100) one click away under it, ←/→ (or ↑/↓, or the wheel)
//! to step and M to mute. The level and the mute survive the session: each
//! change is written to `$XDG_STATE_HOME/lunar/volume` (default
//! `~/.local/state/lunar/volume`), and the next session puts them back. The meter's colour climbs from the accent to the
//! warning as the level does, and the whole control goes dim when muted.
//!
//! The output picked is kept too, in `$XDG_STATE_HOME/lunar/output`, by the
//! card's own name and not its number: whether a GPU's HDMI with a monitor
//! is card 0 or 1 changes from boot to boot (`first_kernel_audio_card` in
//! `linux-object/src/fs/mod.rs`), so a saved number would point at the
//! other card half the time. With PulseAudio (the installed desktop) picking
//! is `pactl set-default-sink` plus moving what is playing; without it
//! (no daemon) it is a `~/.asoundrc` whose ALSA `default` is that card.
//!
//! A child of `main.rs` because that file is at its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;

const PW: i32 = 272;
const HEAD_H: i32 = 36;
/// One output's row.
const DEV_H: i32 = 24;
/// At most this many outputs are listed.
pub const MAX_OUTPUTS: usize = 6;
const METER_H: i32 = 28;
const TICK_H: i32 = 20;
const PAD: i32 = 8;
/// The − and + buttons, and the mute switch.
const BTN: i32 = 20;
/// One step of − / +, the arrows and the wheel.
pub const STEP: i32 = 5;
/// The levels under the meter.
pub const PRESETS: [u32; 5] = [0, 25, 50, 75, 100];

/// The panel for `n` outputs (one row even for none, which says so).
pub fn size(n: usize) -> (i32, i32) {
    let rows = n.clamp(1, MAX_OUTPUTS) as i32;
    (PW, HEAD_H + rows * DEV_H + METER_H + TICK_H + PAD)
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
    if let Some(s) = run("pactl", &["get-sink-mute", "@DEFAULT_SINK@"]) {
        return s.contains("yes");
    }
    if let Some(s) = run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]) {
        return s.contains("MUTED");
    }
    run("amixer", &["get", "Master"]).is_some_and(|s| s.contains("[off]"))
}

/// A place the sound can go.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// What picking it sets: a PulseAudio sink's name, or (no daemon) the
    /// ALSA card's number.
    pub id: String,
    /// The card's own name (`alsa.card_name`, or the bracket of `aplay -l`):
    /// what is saved, because the number moves.
    pub card: String,
    /// What the row says.
    pub label: String,
    /// The sound goes here now.
    pub on: bool,
    /// `id` is a PulseAudio sink.
    pub pulse: bool,
}

/// `cmd`'s output, in English whatever the session's language: `pactl`
/// and `aplay` translate their labels ("Nombre:", "tarjeta 0:"), and the
/// parsers read the English ones.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

/// The outputs there are, the one in use marked: PulseAudio's sinks, else
/// ALSA's cards. Empty when the machine has none it can name.
pub fn devices() -> Vec<Output> {
    let es = i18n::Lang::current() == i18n::Lang::Es;
    if let Some(list) = run("pactl", &["list", "sinks"]) {
        let def = run("pactl", &["get-default-sink"]).unwrap_or_default();
        let v = pulse_outputs(&list, def.trim(), es);
        if !v.is_empty() {
            return v;
        }
    }
    let chosen = home_asoundrc().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| asoundrc_card(&s));
    run("aplay", &["-l"]).map(|s| alsa_outputs(&s, chosen, es)).unwrap_or_default()
}

/// The sinks in `pactl list sinks` output, `def` marked, Pulse's stand-in
/// null sink left out (it is what `module-always-sink` makes when there is
/// no card).
fn pulse_outputs(list: &str, def: &str, es: bool) -> Vec<Output> {
    let mut v: Vec<(String, String, String)> = Vec::new();
    for line in list.lines() {
        let l = line.trim();
        if let Some(n) = l.strip_prefix("Name:") {
            v.push((n.trim().to_string(), String::new(), String::new()));
        } else if let Some(cur) = v.last_mut() {
            if let Some(d) = l.strip_prefix("Description:") {
                cur.1 = d.trim().to_string();
            } else if let Some(c) = prop(l, "alsa.card_name") {
                cur.2 = c;
            }
        }
    }
    let outs = v
        .into_iter()
        .filter(|(n, ..)| !n.is_empty() && n != "auto_null")
        .map(|(id, desc, card)| Output {
            label: label(&card, &desc, es),
            card: if card.is_empty() { id.clone() } else { card },
            on: id == def,
            id,
            pulse: true,
        })
        .collect();
    distinct(outs)
}

/// The cards in `aplay -l` output (`card 0: EclipseHDA0 [hda-01:00.1
/// (nvidia-hdmi)], device 0: ...`), `chosen` (else card 0) marked.
fn alsa_outputs(s: &str, chosen: Option<u32>, es: bool) -> Vec<Output> {
    let mut outs: Vec<Output> = Vec::new();
    for l in s.lines() {
        let Some(rest) = l.strip_prefix("card ") else { continue };
        let Some((num, rest)) = rest.split_once(':') else { continue };
        let Ok(n) = num.trim().parse::<u32>() else { continue };
        if outs.iter().any(|o| o.id == n.to_string()) {
            continue; // a second device of the same card
        }
        let card = rest
            .split_once('[')
            .and_then(|(_, r)| r.split_once("],").or_else(|| r.rsplit_once(']')))
            .map(|(c, _)| c.trim().to_string())
            .unwrap_or_default();
        outs.push(Output {
            id: n.to_string(),
            label: label(&card, "", es),
            card,
            on: n == chosen.unwrap_or(0),
            pulse: false,
        });
    }
    if !outs.iter().any(|o| o.on) {
        if let Some(o) = outs.first_mut() {
            o.on = true; // the saved card is gone: ALSA falls back to card 0's file
        }
    }
    distinct(outs)
}

/// What a row calls a card: what it is (an HDMI, the board's jacks) rather
/// than the PCI place the kernel names it by. Pulse's own description is
/// used only when it is not one Eclipse's `system.pa` set by card number
/// ("Eclipse" for card 0, "Analog" for card 1), which says nothing of which
/// card that is today.
fn label(card: &str, desc: &str, es: bool) -> String {
    let c = card.to_ascii_lowercase();
    if c.contains("hdmi") || c.contains("displayport") {
        return if c.contains("nvidia") { "HDMI (NVIDIA)" } else { "HDMI" }.to_string();
    }
    if c.contains("udrv-hda") {
        return if es { "Salida analógica" } else { "Analog out" }.to_string();
    }
    if !desc.is_empty() && desc != "Eclipse" && desc != "Analog" {
        return desc.to_string();
    }
    if !card.is_empty() {
        return card.to_string();
    }
    if desc.is_empty() { "?" } else { desc }.to_string()
}

/// Two rows never say the same: the second of a name gets a number.
fn distinct(mut v: Vec<Output>) -> Vec<Output> {
    let base: Vec<String> = v.iter().map(|o| o.label.clone()).collect();
    for (i, o) in v.iter_mut().enumerate() {
        let before = base[..i].iter().filter(|b| **b == base[i]).count();
        if before > 0 {
            o.label = format!("{} {}", base[i], before + 1);
        }
    }
    v
}

/// A `key = "value"` property (`wpctl inspect`, `pactl list`).
fn prop(s: &str, key: &str) -> Option<String> {
    s.lines().find_map(|l| {
        let l = l.trim().trim_start_matches('*').trim();
        let v = l.strip_prefix(key)?.trim().strip_prefix('=')?.trim();
        Some(v.trim_matches('"').to_string()).filter(|v| !v.is_empty())
    })
}

/// The user's `~/.asoundrc`, where the no-daemon pick goes.
fn home_asoundrc() -> Option<std::path::PathBuf> {
    let h = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(std::path::Path::new(&h).join(".asoundrc"))
}

/// The first line of an `.asoundrc` this control wrote: someone else's file
/// is never overwritten.
const ASOUNDRC_MARK: &str = "# lunarbar-output";

/// The `.asoundrc` that makes card `n` ALSA's `default`.
fn asoundrc(n: u32) -> String {
    format!(
        "{ASOUNDRC_MARK} card={n}\n\
         # Written by the bar's volume control (the output picked there).\n\
         # Delete this file to go back to card 0.\n\
         pcm.!default {{\n    type plug\n    slave.pcm \"hw:{n},0\"\n}}\n\
         ctl.!default {{\n    type hw\n    card {n}\n}}\n"
    )
}

/// The card an `.asoundrc` of ours picks; `None` for anyone else's.
fn asoundrc_card(s: &str) -> Option<u32> {
    s.lines().next()?.strip_prefix(ASOUNDRC_MARK)?.trim().strip_prefix("card=")?.parse().ok()
}

/// Two outputs, the analog in use: what `LUNARBAR_DUMP_VOL` draws.
pub fn sample_outputs() -> Vec<Output> {
    let es = i18n::Lang::current() == i18n::Lang::Es;
    let o = |id: &str, card: &str, on| Output { id: id.into(), label: label(card, "", es), card: card.into(), on, pulse: true };
    vec![o("alsa_output.hw_0_0", "hda-01:00.1 (nvidia-hdmi)", false), o("analog", "Eclipse HDA (udrv-hda)", true)]
}

/// A sink name safe to hand to the shell between single quotes.
fn shell_safe(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._:-@".contains(c))
}

/// The shell that sends the sound to `o`, then puts level `v` and `muted`
/// on it, so the volume shown is the volume heard. `None` when it cannot
/// be done by command (the `.asoundrc` is written by [`pick`] itself).
fn pick_cmd(o: &Output, v: u32, muted: bool) -> Option<String> {
    let tail = format!("{} ; {}", set_cmd(v), mute_cmd(muted));
    if !o.pulse {
        return Some(format!("amixer -c {} set Master {v}% >/dev/null 2>&1 ; amixer -c {} set Master {} >/dev/null 2>&1", o.id, o.id, if muted { "mute" } else { "unmute" }));
    }
    if !shell_safe(&o.id) {
        return None;
    }
    let s = &o.id;
    // Streams already playing stay on the sink they started on unless moved.
    Some(format!(
        "pactl set-default-sink '{s}' ; for i in $(pactl list short sink-inputs | cut -f1) ; do pactl move-sink-input \"$i\" '{s}' ; done ; {tail}"
    ))
}

/// Send the sound to `o`: the `.asoundrc` for ALSA, the command (returned,
/// for the caller to spawn) for both.
fn pick(o: &Output, v: u32, muted: bool) -> Option<String> {
    if !o.pulse {
        let n: u32 = o.id.parse().ok()?;
        let p = home_asoundrc()?;
        match std::fs::read_to_string(&p) {
            Ok(s) if asoundrc_card(&s).is_none() => return None, // someone else's file
            _ => {}
        }
        if n == 0 {
            let _ = std::fs::remove_file(&p); // card 0 is the system's own default
        } else if std::fs::write(&p, asoundrc(n)).is_err() {
            return None;
        }
    }
    pick_cmd(o, v, muted)
}

/// Where the output picked is kept between sessions.
fn output_path() -> Option<std::path::PathBuf> {
    Some(saved_path()?.with_file_name("output"))
}

fn encode_output(card: &str) -> String {
    format!("card={}\n", card.replace('\n', " "))
}

fn decode_output(s: &str) -> Option<String> {
    s.lines().find_map(|l| l.strip_prefix("card=")).map(|c| c.trim().to_string()).filter(|c| !c.is_empty())
}

/// The output among `outs` the last session picked, when it is not the one
/// in use already.
fn to_restore<'a>(outs: &'a [Output], card: &str) -> Option<&'a Output> {
    outs.iter().find(|o| o.card == card).filter(|o| !o.on)
}

/// Paint the control and return its hitboxes.
pub fn draw(
    cv: &mut Canvas,
    px: i32,
    py: i32,
    pw: i32,
    v: u32,
    muted: bool,
    devices: &[Output],
) -> Vec<(i32, i32, i32, i32, Action)> {
    draw_panel(cv, px, py, pw, size(devices.len()).1, 10);
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

    // Where the sound can go: one row an output, the one in use lit. A
    // click on another sends the sound there.
    let max = ((pw - 44) / GLYPH_W) as usize;
    let cut = |s: &str| -> String {
        if s.chars().count() > max {
            s.chars().take(max.saturating_sub(1)).chain(['.']).collect()
        } else {
            s.to_string()
        }
    };
    if devices.is_empty() {
        let none = if es { "sin dispositivo" } else { "no device" };
        cv.text(none, px + 14, py + HEAD_H + (DEV_H - GLYPH_H) / 2, pal().muted);
    }
    for (i, o) in devices.iter().take(MAX_OUTPUTS).enumerate() {
        let ry = py + HEAD_H + i as i32 * DEV_H;
        let d = 10;
        let (dx, dy) = (px + 14, ry + (DEV_H - d) / 2);
        if o.on {
            cv.round_rect(dx, dy, d, d, d / 2, pal().accent);
        } else {
            cv.round_rect(dx, dy, d, d, d / 2, pal().rule);
            cv.round_rect(dx + 2, dy + 2, d - 4, d - 4, (d - 4) / 2, pal().pill);
        }
        let fg = if o.on { pal().text } else { pal().muted };
        cv.text(&cut(&o.label), px + 32, ry + (DEV_H - GLYPH_H) / 2, fg);
        hits.push((px + 8, ry, px + pw - 8, ry + DEV_H, Action::VolumeOutput(i as u8)));
    }

    // − meter +
    let top = py + HEAD_H + devices.len().clamp(1, MAX_OUTPUTS) as i32 * DEV_H;
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
    ///
    /// And the output it picked, first, so the level lands on that one. On
    /// a thread: the sound server may not answer yet when the bar starts,
    /// so it is asked again for a while.
    pub(crate) fn restore_volume(&mut self) {
        let level = saved();
        if let Some((v, m)) = level {
            self.vol = Some(v);
            self.vol_muted = m;
        }
        let card = output_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| decode_output(&s));
        let Some(card) = card else {
            if let Some((v, m)) = level {
                self.spawn(&format!("{} ; {}", set_cmd(v), mute_cmd(m)));
            }
            return;
        };
        let (v, m) = level.unwrap_or((self.vol.unwrap_or(50), self.vol_muted));
        let _ = std::thread::Builder::new().name("lunarbar-output".into()).spawn(move || {
            for _ in 0..15 {
                let outs = devices();
                if !outs.is_empty() {
                    let cmd = to_restore(&outs, &card).and_then(|o| pick(o, v, m));
                    let cmd = cmd.unwrap_or_else(|| format!("{} ; {}", set_cmd(v), mute_cmd(m)));
                    lunarbar::proc::spawn_detached(&cmd);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        });
    }

    /// Send the sound to the popup's output `i`, and remember it.
    pub(crate) fn pick_output(&mut self, qh: &QueueHandle<State>, i: usize) {
        let (v, m) = (self.vol.unwrap_or(0), self.vol_muted);
        let Some(Popup { kind: PopupKind::Volume { devices, .. }, .. }) = self.popup.as_mut() else {
            return;
        };
        let Some(o) = devices.get(i).cloned() else {
            return;
        };
        if o.on {
            return;
        }
        let Some(cmd) = pick(&o, v, m) else {
            return;
        };
        for (j, d) in devices.iter_mut().enumerate() {
            d.on = j == i;
        }
        self.spawn(&cmd);
        if let Some(p) = output_path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, encode_output(&o.card));
        }
        self.render_popup(qh);
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
            Action::VolumeOutput(i) => self.pick_output(qh, i as usize),
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

    /// What `pactl list sinks` printed on a board whose NVIDIA HDMI had a
    /// monitor (card 0, the kernel's) and whose Realtek is served (card 1).
    const SINKS: &str = "Sink #0\n\tState: IDLE\n\tName: alsa_output.hw_0_0\n\tDescription: Eclipse\n\tProperties:\n\t\talsa.card = \"0\"\n\t\talsa.card_name = \"hda-01:00.1 (nvidia-hdmi)\"\n\t\tdevice.description = \"Eclipse\"\nSink #1\n\tName: analog\n\tDescription: Analog\n\tProperties:\n\t\talsa.card = \"1\"\n\t\talsa.card_name = \"Eclipse HDA (udrv-hda)\"\n";

    #[test]
    fn the_sinks_are_named_by_what_they_are_not_their_number() {
        let v = pulse_outputs(SINKS, "analog", true);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].label.as_str(), v[0].on), ("HDMI (NVIDIA)", false));
        assert_eq!((v[1].label.as_str(), v[1].on), ("Salida analógica", true));
        assert_eq!(v[1].card, "Eclipse HDA (udrv-hda)");
        assert_eq!(pulse_outputs(SINKS, "alsa_output.hw_0_0", false)[1].label, "Analog out");
        // A sink with a description of its own keeps it; the null sink is not an output.
        let bt = "Sink #3\n\tName: bluez.b\n\tDescription: Auriculares BT\nSink #4\n\tName: auto_null\n\tDescription: Dummy Output\n";
        let v = pulse_outputs(bt, "bluez.b", true);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].label.as_str(), v[0].card.as_str(), v[0].on), ("Auriculares BT", "bluez.b", true));
    }

    #[test]
    fn the_saved_card_is_found_wherever_its_number_went() {
        // Saved while the analog was card 1; this boot the HDMI had no
        // monitor, so the analog is card 0 and the HDMI card 1.
        let swapped = SINKS
            .replace("hda-01:00.1 (nvidia-hdmi)", "TMP")
            .replace("Eclipse HDA (udrv-hda)", "hda-01:00.1 (nvidia-hdmi)")
            .replace("TMP", "Eclipse HDA (udrv-hda)");
        let outs = pulse_outputs(&swapped, "analog", true);
        let saved = decode_output(&encode_output("Eclipse HDA (udrv-hda)")).unwrap();
        let o = to_restore(&outs, &saved).expect("the analog is not the default");
        assert_eq!(o.id, "alsa_output.hw_0_0");
        // Already there: nothing to do.
        let outs = pulse_outputs(&swapped, "alsa_output.hw_0_0", true);
        assert!(to_restore(&outs, &saved).is_none());
        assert_eq!(decode_output("garbage"), None);
    }

    #[test]
    fn picking_a_sink_moves_the_streams_and_keeps_the_level() {
        let o = &pulse_outputs(SINKS, "analog", true)[0];
        let c = pick_cmd(o, 40, false).unwrap();
        assert!(c.starts_with("pactl set-default-sink 'alsa_output.hw_0_0'"), "{c}");
        assert!(c.contains("move-sink-input \"$i\" 'alsa_output.hw_0_0'"), "{c}");
        assert!(c.contains("set-sink-volume @DEFAULT_SINK@ 40%"), "{c}");
        let evil = Output { id: "x'; rm -rf /".into(), card: String::new(), label: String::new(), on: false, pulse: true };
        assert_eq!(pick_cmd(&evil, 1, false), None);
    }

    #[test]
    fn without_a_daemon_the_cards_come_from_aplay_and_the_pick_is_an_asoundrc() {
        let l = "**** List of PLAYBACK Hardware Devices ****\ncard 0: EclipseHDA0 [hda-01:00.1 (nvidia-hdmi)], device 0: HDA [HDA]\n  Subdevices: 1/1\ncard 1: EclipseHDA1 [Eclipse HDA (udrv-hda)], device 0: PCM [PCM]\ncard 1: EclipseHDA1 [Eclipse HDA (udrv-hda)], device 1: PCM [PCM]\n";
        let v = alsa_outputs(l, None, true);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].id.as_str(), v[0].label.as_str(), v[0].on), ("0", "HDMI (NVIDIA)", true));
        assert_eq!((v[1].card.as_str(), v[1].on), ("Eclipse HDA (udrv-hda)", false));
        let v = alsa_outputs(l, asoundrc_card(&asoundrc(1)), true);
        assert!(v[1].on && !v[0].on);
        assert!(asoundrc(1).contains("slave.pcm \"hw:1,0\""));
        assert_eq!(asoundrc_card("pcm.!default { type hw card 1 }\n"), None);
        // A card that went away: card 0 is what ALSA plays to.
        assert!(alsa_outputs(l, Some(5), true)[0].on);
    }

    #[test]
    fn two_cards_of_a_kind_get_numbers() {
        let s = "card 0: A [hda-01:00.1 (nvidia-hdmi)], device 0: x\ncard 1: B [hda-02:00.1 (nvidia-hdmi)], device 0: x\n";
        let v = alsa_outputs(s, None, true);
        assert_eq!(v[0].label, "HDMI (NVIDIA)");
        assert_eq!(v[1].label, "HDMI (NVIDIA) 2");
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
        let outs = pulse_outputs(SINKS, "analog", true);
        let (pw, ph) = size(outs.len());
        let mut cv = Canvas::try_new((pw + 2 * px) as usize, (ph + 2 * py) as usize).unwrap();
        let hits = draw(&mut cv, px, py, pw, 42, false, &outs);
        assert_eq!(hits.iter().filter(|h| matches!(h.4, Action::VolumeOutput(_))).count(), 2);
        for (x0, y0, x1, y1, a) in hits {
            assert!(x0 >= px && y0 >= py && x1 <= px + pw && y1 <= py + ph, "{x0},{y0}..{x1},{y1}");
            let _ = a;
        }
    }
}
