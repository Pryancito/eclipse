//! Keybindings — formato labwc (`W-q`, `A-F4`, `C-A-Delete`, …).
//!
//! Modificadores:
//!   * `W` → Super (Mod4)        — la "tecla Windows" / Logo
//!   * `A` → Alt   (Mod1)
//!   * `C` → Ctrl
//!   * `S` → Shift
//!
//! Las teclas son nombres `xkb` (`Tab`, `Return`, `q`, `F4`, `Left`, …).

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use bitflags::bitflags;

use crate::actions::Action;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Mods: u8 {
        const SUPER = 0b0001;   // W
        const ALT   = 0b0010;   // A
        const CTRL  = 0b0100;   // C
        const SHIFT = 0b1000;   // S
    }
}

#[derive(Clone, Debug)]
pub struct Keybind {
    pub mods: Mods,
    /// Keysym xkb-style ya canonicalizado a minúsculas (excepto F1..F12 / Special).
    pub key:  String,
    pub action: Action,
}

/// `parse_keybind("W-q", Action::Close)` → Keybind con SUPER+q.
pub fn parse_keybind(spec: &str, action: Action) -> Option<Keybind> {
    let parts: Vec<&str> = spec.split('-').collect();
    if parts.is_empty() { return None; }
    let key = parts.last()?.trim();
    if key.is_empty() { return None; }
    let mut mods = Mods::empty();
    for m in &parts[..parts.len()-1] {
        match m.trim() {
            "W" | "Super" | "Mod4" => mods |= Mods::SUPER,
            "A" | "Alt"   | "Mod1" => mods |= Mods::ALT,
            "C" | "Ctrl"           => mods |= Mods::CTRL,
            "S" | "Shift"          => mods |= Mods::SHIFT,
            _ => return None,
        }
    }
    Some(Keybind { mods, key: key.to_string(), action })
}

/// Defaults clásicos labwc 0.8 (mismo set que viene en `docs/rc.xml.in`).
pub fn default_keybinds() -> Vec<Keybind> {
    use Action::*;
    let entries = [
        ("A-Tab",       NextWindow),
        ("A-S-Tab",     PreviousWindow),
        ("W-q",         Close),
        ("W-Up",        Maximize),
        ("W-Down",      Iconify),
        ("W-Return",    Execute("eclipse-terminal".into())),
        ("W-d",         Execute("eclipse-launcher".into())),
        ("C-A-Delete",  Exit),
        ("W-S-r",       Reconfigure),
        ("W-1",         GoToDesktop(1)),
        ("W-2",         GoToDesktop(2)),
        ("W-3",         GoToDesktop(3)),
        ("W-4",         GoToDesktop(4)),
    ];
    entries.into_iter()
        .filter_map(|(s, a)| parse_keybind(s, a))
        .collect()
}

/// Coteja una pulsación contra la lista de keybinds y devuelve la action a ejecutar.
pub fn match_keybind<'a>(binds: &'a [Keybind], mods: Mods, key: &str) -> Option<&'a Action> {
    binds.iter().find(|kb| kb.mods == mods && kb.key.eq_ignore_ascii_case(key)).map(|kb| &kb.action)
}

/// Conversión de evdev keycode (Eclipse `input:` scheme usa PS/2 +8) a un
/// nombre xkb mínimo. Cubrimos las teclas más usadas en keybinds.
pub fn evdev_to_xkb(code: u32) -> Option<&'static str> {
    Some(match code {
        1   => "Escape",
        14  => "BackSpace",
        15  => "Tab",
        28  => "Return",
        29  => "Control_L",
        42  => "Shift_L",
        54  => "Shift_R",
        56  => "Alt_L",
        57  => "space",
        58  => "Caps_Lock",
        59..=68 => match code { 59=>"F1",60=>"F2",61=>"F3",62=>"F4",63=>"F5",64=>"F6",65=>"F7",66=>"F8",67=>"F9",68=>"F10", _=>"" },
        87  => "F11",
        88  => "F12",
        103 => "Up",
        108 => "Down",
        105 => "Left",
        106 => "Right",
        110 => "Insert",
        111 => "Delete",
        102 => "Home",
        107 => "End",
        125 => "Super_L",
        16..=25 => match code { 16=>"q",17=>"w",18=>"e",19=>"r",20=>"t",21=>"y",22=>"u",23=>"i",24=>"o",25=>"p", _=>"" },
        30..=38 => match code { 30=>"a",31=>"s",32=>"d",33=>"f",34=>"g",35=>"h",36=>"j",37=>"k",38=>"l", _=>"" },
        44..=50 => match code { 44=>"z",45=>"x",46=>"c",47=>"v",48=>"b",49=>"n",50=>"m", _=>"" },
        2..=11 => match code { 2=>"1",3=>"2",4=>"3",5=>"4",6=>"5",7=>"6",8=>"7",9=>"8",10=>"9",11=>"0", _=>"" },
        _   => return None,
    })
}
