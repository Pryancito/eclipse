//! `themerc` — formato Openbox heredado por labwc.
//!
//! Lee colores/bordes/fuentes de
//! `~/.local/share/themes/<NAME>/openbox-3/themerc` o
//! `/usr/share/themes/<NAME>/openbox-3/themerc`.
//!
//! Parser muy mínimo: pares `key: value`, comentarios con `#`. Ignora claves
//! desconocidas. Defaults razonables (tema "Default" inspirado en Adwaita).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self { Self(r, g, b, 0xFF) }
    pub fn argb_u32(self) -> u32 {
        ((self.3 as u32) << 24) | ((self.0 as u32) << 16) | ((self.1 as u32) << 8) | (self.2 as u32)
    }
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    /// Padding de la barra de título (px).
    pub title_height: i32,
    /// Grosor del borde activo / inactivo (px).
    pub border_width: i32,
    pub corner_radius: i32,

    pub border_active:    Rgba,
    pub border_inactive:  Rgba,

    pub title_bg_active:   Rgba,
    pub title_bg_inactive: Rgba,
    pub title_fg_active:   Rgba,
    pub title_fg_inactive: Rgba,

    pub menu_bg:   Rgba,
    pub menu_fg:   Rgba,
    pub menu_sel:  Rgba,

    pub btn_close:  Rgba,
    pub btn_max:    Rgba,
    pub btn_min:    Rgba,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            title_height: 28,
            border_width: 1,
            corner_radius: 6,

            border_active:    Rgba::rgb(0x35, 0x84, 0xE4),
            border_inactive:  Rgba::rgb(0x55, 0x55, 0x55),

            title_bg_active:   Rgba::rgb(0x24, 0x24, 0x29),
            title_bg_inactive: Rgba::rgb(0x18, 0x18, 0x1B),
            title_fg_active:   Rgba::rgb(0xEE, 0xEE, 0xEE),
            title_fg_inactive: Rgba::rgb(0x99, 0x99, 0x99),

            menu_bg:   Rgba::rgb(0x2A, 0x2A, 0x2E),
            menu_fg:   Rgba::rgb(0xEE, 0xEE, 0xEE),
            menu_sel:  Rgba::rgb(0x35, 0x84, 0xE4),

            btn_close: Rgba::rgb(0xE0, 0x4B, 0x4B),
            btn_max:   Rgba::rgb(0x4B, 0xC0, 0x6E),
            btn_min:   Rgba::rgb(0xE0, 0xC4, 0x4B),
        }
    }
}

impl Theme {
    pub fn load(name: &str) -> Self {
        let mut paths: Vec<alloc::string::String> = Vec::new();
        paths.push(alloc::format!("/usr/share/themes/{name}/openbox-3/themerc"));
        paths.push(alloc::format!("/etc/labwc/themes/{name}/themerc"));
        for p in paths.iter() {
            if let Some(data) = read_file(p) {
                if let Ok(s) = core::str::from_utf8(&data) {
                    let mut t = Theme::default();
                    t.name = name.to_string();
                    apply_themerc(&mut t, s);
                    return t;
                }
            }
        }
        let mut t = Theme::default();
        t.name = name.to_string();
        t
    }
}

fn apply_themerc(t: &mut Theme, s: &str) {
    for line in s.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() { continue; }
        let (k, v) = match line.split_once(':') { Some((a,b)) => (a.trim(), b.trim()), None => continue };
        match k {
            "window.active.border.color"      => if let Some(c)=parse_color(v) { t.border_active   = c; }
            "window.inactive.border.color"    => if let Some(c)=parse_color(v) { t.border_inactive = c; }
            "window.active.title.bg.color"    => if let Some(c)=parse_color(v) { t.title_bg_active   = c; }
            "window.inactive.title.bg.color"  => if let Some(c)=parse_color(v) { t.title_bg_inactive = c; }
            "window.active.label.text.color"  => if let Some(c)=parse_color(v) { t.title_fg_active   = c; }
            "window.inactive.label.text.color"=> if let Some(c)=parse_color(v) { t.title_fg_inactive = c; }
            "menu.items.bg.color"             => if let Some(c)=parse_color(v) { t.menu_bg  = c; }
            "menu.items.text.color"           => if let Some(c)=parse_color(v) { t.menu_fg  = c; }
            "menu.items.active.bg.color"      => if let Some(c)=parse_color(v) { t.menu_sel = c; }
            "border.width"                    => t.border_width = v.parse().unwrap_or(t.border_width),
            "window.label.text.justify"       => { /* TODO */ }
            "window.handle.height"            => t.title_height = v.parse().unwrap_or(t.title_height),
            _ => {}
        }
    }
}

fn parse_color(s: &str) -> Option<Rgba> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let bytes = hex.as_bytes();
        let parse = |i: usize| -> Option<u8> {
            let h = core::str::from_utf8(&bytes[i..i+2]).ok()?;
            u8::from_str_radix(h, 16).ok()
        };
        if hex.len() == 6 { return Some(Rgba::rgb(parse(0)?, parse(2)?, parse(4)?)); }
        if hex.len() == 8 { return Some(Rgba(parse(0)?, parse(2)?, parse(4)?, parse(6)?)); }
    }
    None
}

fn read_file(path: &str) -> Option<Vec<u8>> {
    use libc::{open, read, close, O_RDONLY};
    let mut cstr = path.as_bytes().to_vec();
    cstr.push(0);
    let fd = unsafe { open(cstr.as_ptr() as *const core::ffi::c_char, O_RDONLY, 0) };
    if fd < 0 { return None; }
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = unsafe { read(fd, tmp.as_mut_ptr() as *mut core::ffi::c_void, tmp.len()) };
        if n <= 0 { break; }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    unsafe { close(fd); }
    Some(buf)
}
