//! The "Servicios" page of the control panel: eclipse-init's services, read
//! from their directory (`/etc/eclipse/services`) and from what init says of
//! them (`/run/eclipse-init/services`), and whether each one starts at boot.
//!
//! Init loads every `NAME.service` in the directory when it starts, and
//! nothing else. So a service that should not start at boot is renamed
//! `NAME.service.off`: init no longer sees it, and nothing in init has to
//! change. Starting, stopping and restarting one now goes through init's
//! control directory ([`crate::sysmon::request`]), as lunarmon does.
//!
//! What a service is said to be is its file's first comment line; which
//! ones the page will not turn off is the page's block (`esenciales`).

use std::path::{Path, PathBuf};

use crate::sysmon::Svc;

/// The extension of a service turned off at boot.
pub const OFF: &str = ".off";

/// One service, from its file and from init.
#[derive(Clone, Debug, PartialEq)]
pub struct Servicio {
    pub nombre: String,
    /// What its file's first comment line says it is.
    pub que: String,
    pub exec: String,
    /// `respawn` or `oneshot`.
    pub tipo: String,
    /// It has `desktop =`: part of a desktop session.
    pub escritorio: bool,
    /// It drives a device (`udriver =`).
    pub udriver: Option<String>,
    /// Where its output goes, when its file says (`log =`).
    pub log: Option<String>,
    /// `NAME.service`, not `NAME.service.off`: init starts it at boot.
    pub al_arrancar: bool,
    /// The page will not turn it off.
    pub esencial: bool,
    /// What init says of it now, when it loaded it.
    pub ahora: Option<Svc>,
}

/// What a page row says of a service's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Estado {
    EnMarcha,
    /// Waiting for its turn, or for what it needs.
    Esperando,
    /// Stopped by hand, or a oneshot that has finished.
    Parado,
    /// Init gave up on it after too many quick deaths.
    Abandonado,
    /// Not loaded this boot: turned off, or added since.
    NoCargado,
}

impl Servicio {
    pub fn estado(&self) -> Estado {
        match self.ahora.as_ref().map(|s| s.state.as_str()) {
            None => Estado::NoCargado,
            Some("running") => Estado::EnMarcha,
            Some("waiting") => Estado::Esperando,
            Some("given-up") => Estado::Abandonado,
            Some(_) => Estado::Parado,
        }
    }

    /// The line under its name: what it is and its kind.
    pub fn sub(&self) -> String {
        match (&self.udriver, self.que.is_empty()) {
            (Some(d), _) => format!("driver · {d}"),
            (None, true) => self.tipo.clone(),
            (None, false) => format!("{} · {}", self.que, self.tipo),
        }
    }

    /// Where its output is: what init says, else its file, else init's
    /// default for a service (`/tmp/NAME.log`).
    pub fn registro(&self) -> String {
        self.ahora
            .as_ref()
            .and_then(|s| s.log.clone())
            .or_else(|| self.log.clone())
            .unwrap_or_else(|| format!("/tmp/{}.log", self.nombre))
    }

    /// The file it is in, as it is now.
    pub fn fichero(&self, dir: &Path) -> PathBuf {
        let name = format!("{}.service", self.nombre);
        if self.al_arrancar {
            dir.join(name)
        } else {
            dir.join(name + OFF)
        }
    }
}

/// A service file's text, as the page needs it.
fn lee_fichero(nombre: &str, text: &str, al_arrancar: bool) -> Servicio {
    let mut s = Servicio {
        nombre: nombre.to_string(),
        que: String::new(),
        exec: String::new(),
        tipo: "oneshot".into(),
        escritorio: false,
        udriver: None,
        log: None,
        al_arrancar,
        esencial: false,
        ahora: None,
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(c) = line.strip_prefix('#') {
            if s.que.is_empty() && s.exec.is_empty() {
                // Its first sentence: "Abyss Wayland session. See …".
                let c = c.trim();
                let c = c.split_once(". ").map_or(c, |(a, _)| a);
                s.que = c.trim_end_matches(['.', ':']).to_string();
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "exec" => s.exec = v,
            "type" => s.tipo = v,
            "desktop" => s.escritorio = true,
            "udriver" => s.udriver = Some(v),
            "log" => s.log = Some(v),
            _ => {}
        }
    }
    s
}

/// The services in `dir`, on and off, joined with what init says of them
/// (`svcs`), by name. `esenciales` are the ones the page will not turn off.
pub fn lee(dir: &Path, svcs: &[Svc], esenciales: &[&str]) -> Vec<Servicio> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let file = e.file_name().to_string_lossy().into_owned();
        let (nombre, on) = match file.strip_suffix(OFF) {
            Some(f) => (f.strip_suffix(".service"), false),
            None => (file.strip_suffix(".service"), true),
        };
        let Some(nombre) = nombre.filter(|n| nombre_ok(n)) else {
            continue;
        };
        // Both there: the one init loads is the one that counts.
        if !on && dir.join(format!("{nombre}.service")).exists() {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        let mut s = lee_fichero(nombre, &text, on);
        s.esencial = esenciales.contains(&nombre);
        s.ahora = svcs.iter().find(|x| x.name == nombre).cloned();
        out.push(s);
    }
    out.sort_by(|a, b| a.nombre.cmp(&b.nombre));
    out
}

/// A name init could have loaded: what its control directory takes.
pub fn nombre_ok(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

/// Turns `nombre` on (`NAME.service`) or off (`NAME.service.off`) at boot,
/// in `dir`. An essential one is never turned off.
pub fn al_arrancar(dir: &Path, s: &Servicio, on: bool) -> Result<(), String> {
    if !nombre_ok(&s.nombre) {
        return Err(format!("«{}» no es un servicio", s.nombre));
    }
    if !on && s.esencial {
        return Err(format!("{} es esencial", s.nombre));
    }
    if s.al_arrancar == on {
        return Ok(());
    }
    let on_path = dir.join(format!("{}.service", s.nombre));
    let off_path = dir.join(format!("{}.service{OFF}", s.nombre));
    let (from, to) = if on {
        (off_path, on_path)
    } else {
        (on_path, off_path)
    };
    std::fs::rename(&from, &to).map_err(|e| format!("{}: {e}", from.display()))
}

/// The time since `started` (a wall-clock second), as the page says it.
pub fn desde(started: Option<u64>, now: u64) -> String {
    let Some(t) = started.filter(|t| *t <= now) else {
        return "—".into();
    };
    let s = now - t;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
#[path = "servicios_tests.rs"]
mod tests;
