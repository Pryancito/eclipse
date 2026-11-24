//! The "claves" of Eclipse OS as DATA: what the control panel (lunarpanel)
//! reads to build itself.
//!
//! Every setting the panel shows is a block in a file under
//! `/usr/share/eclipse/claves.d/` (and `/etc/eclipse/claves.d/`, for the ones a
//! machine adds). A block says which file and which `key=` it lives in, what
//! kind of value it takes, which values are allowed and, optionally, which
//! command applies it. Nothing about the panel's screen is written down
//! anywhere: the groups, the controls, their widths and the label column are
//! all COMPUTED from these blocks (see [`layout`] in the binary). A new
//! setting is a new block; the panel is not rebuilt for it.
//!
//! ```text
//! [grupo:apariencia]
//! es = Apariencia
//! en = Appearance
//!
//! [aspecto]
//! grupo   = apariencia
//! fichero = /etc/eclipse/look
//! clave   = look
//! tipo    = elegir
//! valores = tecno abyss eclipse kde win11 gnome
//! aplicar = /usr/local/bin/eclipse-look {}
//! es      = Aspecto del escritorio
//! en      = Desktop look
//! ```
//!
//! Types: `elegir` (one of `valores`), `si-no` (two `valores`, on first),
//! `numero` (`rango = a..b`, optional `paso`) and `abrir` (a button that runs
//! `orden`, for a setting that has a window of its own, like the network).
//!
//! Optional fields: `formato = palabra` (the file holds the value alone, like
//! `/etc/eclipse/desktop`), `montar = /boot/efi` (mount that volume through
//! fstab before touching the file), `leer` (a command that prints the value
//! in force), `reabrir = si` (the panel reopens after applying it) and
//! `reinicio = si` (the value is read at boot).
//!
//! `arranque` edits one item of a kernel command line held in `clave`
//! (rboot.conf's `cmdline=`, whose LAST line is the one rboot reads):
//! `arranque = LOG` reads and writes `LOG=value`, `arranque = bandera` makes
//! one of `valores` the token present (`nvidia.wlr_vulkan`). The kernel
//! reads the first spelling of a key, so the first one is the one edited and
//! later ones are dropped; `auto` removes the item and leaves the default.
//!
//! Fail-closed on bad data: a block the panel cannot use is LEFT OUT and
//! named in [`Catalogo::avisos`], which the panel's footer counts. A value
//! the file holds that is not one of `valores` reads as `defecto`. A value is
//! only ever written if it is one of the block's own values (or inside its
//! range), so a command line built from it never carries anything the data did
//! not already spell out.

use std::path::{Path, PathBuf};

/// Where the shipped blocks live, and where a machine adds its own.
pub const DIRS: [&str; 2] = ["/usr/share/eclipse/claves.d", "/etc/eclipse/claves.d"];

/// The blocks the image ships (`tools/lunarbar/claves.d`), built in: what
/// `--dump` draws, what the tests check, and what the panel falls back to
/// on a system where none were installed.
pub const DE_SERIE: &[(&str, &str)] = &[
    (
        "10-apariencia.claves",
        include_str!("../claves.d/10-apariencia.claves"),
    ),
    (
        "20-idioma.claves",
        include_str!("../claves.d/20-idioma.claves"),
    ),
    (
        "30-fecha.claves",
        include_str!("../claves.d/30-fecha.claves"),
    ),
    (
        "40-sistema.claves",
        include_str!("../claves.d/40-sistema.claves"),
    ),
    (
        "50-arranque.claves",
        include_str!("../claves.d/50-arranque.claves"),
    ),
    (
        "60-escritorio.claves",
        include_str!("../claves.d/60-escritorio.claves"),
    ),
    (
        "70-avanzado.claves",
        include_str!("../claves.d/70-avanzado.claves"),
    ),
    (
        "80-dispositivos.claves",
        include_str!("../claves.d/80-dispositivos.claves"),
    ),
    (
        "82-servicios.claves",
        include_str!("../claves.d/82-servicios.claves"),
    ),
    (
        "84-aperturas.claves",
        include_str!("../claves.d/84-aperturas.claves"),
    ),
];

#[derive(Clone, Debug, PartialEq)]
pub enum Tipo {
    Elegir,
    SiNo,
    Numero { min: i64, max: i64, paso: i64 },
    Abrir,
    /// The page of the machine's devices and their drivers
    /// ([`crate::dispositivos`]): its `fichero`/`montar`/`clave` say where
    /// the kernel command line is, for the `udriver=` a driver is turned on
    /// with.
    Dispositivos,
    /// The page of eclipse-init's services ([`crate::servicios`]):
    /// `fichero` is their directory, `esenciales` the ones the page will
    /// not turn off (kept as its values).
    Servicios,
    /// The page of what opens each kind of file ([`crate::aperturas`]):
    /// `fichero` is `/etc/eclipse/openers`, `valores` the programs offered.
    Aperturas,
}

/// What of the kernel command line a block edits (see the module notes).
#[derive(Clone, Debug, PartialEq)]
pub enum Arranque {
    /// `KEY=value`, the key compared without case as the kernel does.
    Clave(String),
    /// One of the block's values present as a bare token, or none (`auto`).
    Bandera,
}

/// One allowed value and how it reads in each language.
#[derive(Clone, Debug, PartialEq)]
pub struct Valor {
    pub v: String,
    pub es: String,
    pub en: String,
}

#[derive(Clone, Debug)]
pub struct Clave {
    pub id: String,
    pub grupo: String,
    pub tipo: Tipo,
    pub fichero: Option<PathBuf>,
    pub clave: Option<String>,
    pub valores: Vec<Valor>,
    pub defecto: Option<String>,
    /// Applies a value: `{}` is replaced by it. Without one, the panel writes
    /// `clave=valor` into `fichero` itself.
    pub aplicar: Option<String>,
    /// Prints the value in force, when the file is not where the truth is
    /// (the keyboard layout lives in /proc/kbd).
    pub leer: Option<String>,
    /// What an `abrir` runs.
    pub orden: Option<String>,
    pub es: String,
    pub en: String,
    pub ayuda_es: String,
    pub ayuda_en: String,
    /// `reabrir = si`: the panel opens itself again after applying it, so it
    /// is drawn in the look or the language just chosen.
    pub reabrir: bool,
    /// `formato = palabra`: the file holds the value alone, as its first
    /// word (`/etc/eclipse/desktop`), instead of `clave=valor` lines.
    pub palabra: bool,
    /// `montar = /boot/efi`: the file lives on a volume fstab mounts; when
    /// it is not mounted, fstab's mounting is run before reading or writing.
    pub montar: Option<PathBuf>,
    /// `reinicio = si`: what is written is read at boot, so the row and the
    /// status after applying say it takes effect on the next start.
    pub reinicio: bool,
    /// `arranque = KEY | bandera`: `clave` holds a kernel command line and
    /// the block edits one item of it.
    pub arranque: Option<Arranque>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Grupo {
    pub id: String,
    pub es: String,
    pub en: String,
}

#[derive(Clone, Debug, Default)]
pub struct Catalogo {
    /// In order of first appearance (files sorted by name).
    pub grupos: Vec<Grupo>,
    pub claves: Vec<Clave>,
    /// One line per block left out, and why.
    pub avisos: Vec<String>,
    /// The `[driver:ID]` blocks: which driver takes which device.
    pub drivers: Vec<crate::dispositivos::Driver>,
}

/// A value the data may carry: no spaces, quotes, `$`, `;` or newlines, so
/// it is the same word in a file, on a command line and on screen.
pub fn palabra_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:+-@,".contains(c))
}

impl Clave {
    pub fn nombre(&self, en: bool) -> &str {
        if en && !self.en.is_empty() {
            &self.en
        } else {
            &self.es
        }
    }

    pub fn ayuda(&self, en: bool) -> &str {
        if en && !self.ayuda_en.is_empty() {
            &self.ayuda_en
        } else {
            &self.ayuda_es
        }
    }

    /// `/etc/eclipse/look · look`, what the row says it writes.
    pub fn origen(&self) -> String {
        match (&self.fichero, &self.clave, &self.orden) {
            // The file's name alone: the row has no room for the path and
            // the item, and the item is what tells two rows apart.
            (Some(f), Some(k), _) if self.arranque.is_some() => {
                let f = f.file_name().unwrap_or(f.as_os_str()).to_string_lossy();
                match &self.arranque {
                    Some(Arranque::Clave(a)) => format!("{f} · {k} · {a}"),
                    _ => format!("{f} · {k}"),
                }
            }
            (Some(f), _, _) if self.palabra => f.display().to_string(),
            (Some(f), Some(k), _) => format!("{} · {k}", f.display()),
            (_, _, Some(o)) => o.clone(),
            _ => String::new(),
        }
    }

    /// Whether `v` is something this setting may be set to.
    pub fn admite(&self, v: &str) -> bool {
        match &self.tipo {
            Tipo::Elegir | Tipo::SiNo => self.valores.iter().any(|x| x.v == v),
            Tipo::Numero { min, max, .. } => v.parse::<i64>().is_ok_and(|n| n >= *min && n <= *max),
            Tipo::Abrir | Tipo::Dispositivos | Tipo::Servicios | Tipo::Aperturas => false,
        }
    }

    /// A raw value from a file or a command, as one of this setting's own
    /// values: matched case-blind, else `defecto`, else the first value.
    pub fn normaliza(&self, raw: Option<&str>) -> String {
        let raw = raw.map(str::trim).filter(|s| !s.is_empty());
        match &self.tipo {
            Tipo::Elegir | Tipo::SiNo => {
                let hit = |s: &str| {
                    self.valores
                        .iter()
                        .find(|x| x.v.eq_ignore_ascii_case(s))
                        .map(|x| x.v.clone())
                };
                raw.and_then(hit)
                    .or_else(|| self.defecto.as_deref().and_then(hit))
                    .or_else(|| self.valores.first().map(|x| x.v.clone()))
                    .unwrap_or_default()
            }
            Tipo::Numero { min, max, .. } => {
                let n = raw
                    .and_then(|s| s.parse::<i64>().ok())
                    .or_else(|| self.defecto.as_deref().and_then(|s| s.parse().ok()))
                    .unwrap_or(*min);
                n.clamp(*min, *max).to_string()
            }
            Tipo::Abrir | Tipo::Dispositivos | Tipo::Servicios | Tipo::Aperturas => String::new(),
        }
    }

    /// The value one step from `v`, `dir` = -1 or +1. Choices wrap; numbers
    /// stop at the ends.
    pub fn paso(&self, v: &str, dir: i32) -> String {
        match &self.tipo {
            Tipo::Elegir | Tipo::SiNo => {
                let n = self.valores.len() as i32;
                if n == 0 {
                    return v.to_string();
                }
                let at = self.valores.iter().position(|x| x.v == v).unwrap_or(0) as i32;
                self.valores[(at + dir).rem_euclid(n) as usize].v.clone()
            }
            Tipo::Numero { min, max, paso } => {
                let n = v.parse::<i64>().unwrap_or(*min);
                (n + dir as i64 * paso).clamp(*min, *max).to_string()
            }
            Tipo::Abrir | Tipo::Dispositivos | Tipo::Servicios | Tipo::Aperturas => String::new(),
        }
    }

    pub fn etiqueta<'a>(&'a self, v: &'a str, en: bool) -> &'a str {
        self.valores
            .iter()
            .find(|x| x.v == v)
            .map(|x| if en { x.en.as_str() } else { x.es.as_str() })
            .unwrap_or(v)
    }
}

impl Grupo {
    pub fn nombre(&self, en: bool) -> &str {
        if en && !self.en.is_empty() {
            &self.en
        } else {
            &self.es
        }
    }
}

/// `a:Una_etiqueta b c:Otra` -> values with their labels (`_` is a space).
fn valores(s: &str) -> Option<Vec<(String, String)>> {
    s.split_whitespace()
        .map(|t| {
            let (v, l) = t.split_once(':').unwrap_or((t, t));
            palabra_ok(v).then(|| (v.to_string(), l.replace('_', " ")))
        })
        .collect()
}

fn rango(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once("..")?;
    let (a, b) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a < b).then_some((a, b))
}

/// One `[header]` and its `k = v` lines.
struct Bloque {
    cabecera: String,
    campos: Vec<(String, String)>,
    donde: String,
}

impl Bloque {
    fn get(&self, k: &str) -> Option<&str> {
        self.campos
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

fn bloques(text: &str, origen: &str, avisos: &mut Vec<String>) -> Vec<Bloque> {
    let mut out: Vec<Bloque> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push(Bloque {
                cabecera: h.trim().to_string(),
                campos: Vec::new(),
                donde: format!("{origen}:{}", n + 1),
            });
        } else if let Some((k, v)) = line.split_once('=') {
            match out.last_mut() {
                Some(b) => b.campos.push((k.trim().to_string(), v.trim().to_string())),
                None => avisos.push(format!("{origen}:{}: línea fuera de un bloque", n + 1)),
            }
        } else {
            avisos.push(format!("{origen}:{}: no es «clave = valor»", n + 1));
        }
    }
    out
}

fn clave(b: &Bloque) -> Result<Clave, String> {
    let id = b.cabecera.clone();
    if !palabra_ok(&id) {
        return Err("nombre de bloque no válido".into());
    }
    let grupo = b.get("grupo").ok_or("falta «grupo»")?.to_string();
    let mut tipo = match b.get("tipo").ok_or("falta «tipo»")? {
        "elegir" => Tipo::Elegir,
        "si-no" | "sí-no" => Tipo::SiNo,
        "numero" | "número" => Tipo::Numero {
            min: 0,
            max: 0,
            paso: 1,
        },
        "abrir" => Tipo::Abrir,
        "dispositivos" => Tipo::Dispositivos,
        "servicios" => Tipo::Servicios,
        "aperturas" => Tipo::Aperturas,
        t => return Err(format!("tipo desconocido «{t}»")),
    };
    let fichero = b.get("fichero").map(PathBuf::from);
    let key = b.get("clave").map(str::to_string);
    if let Some(k) = &key {
        if !palabra_ok(k) {
            return Err("«clave» no válida".into());
        }
    }
    let palabra = match b.get("formato") {
        None | Some("clave") => false,
        Some("palabra") => true,
        Some(f) => return Err(format!("formato desconocido «{f}»")),
    };
    let sin_clave = matches!(tipo, Tipo::Servicios | Tipo::Aperturas);
    if !matches!(tipo, Tipo::Abrir) && (fichero.is_none() || (key.is_none() && !palabra && !sin_clave)) {
        return Err("falta «fichero» o «clave»".into());
    }
    let montar = b.get("montar").map(PathBuf::from);
    if montar.as_ref().is_some_and(|m| !m.is_absolute()) {
        return Err("«montar» tiene que ser una ruta absoluta".into());
    }
    if fichero.as_ref().is_some_and(|f| !f.is_absolute()) {
        return Err("«fichero» tiene que ser una ruta absoluta".into());
    }
    // A services page's values are the services it will not turn off.
    let valores_de = if matches!(tipo, Tipo::Servicios) { "esenciales" } else { "valores" };
    let mut vals: Vec<Valor> = match b.get(valores_de) {
        Some(s) => valores(s)
            .ok_or("un valor lleva caracteres que no puede llevar")?
            .into_iter()
            .map(|(v, l)| Valor {
                v,
                es: l.clone(),
                en: l,
            })
            .collect(),
        None => Vec::new(),
    };
    if let Some(s) = b.get("valores.en") {
        for (v, l) in valores(s).ok_or("«valores.en» no válido")? {
            if let Some(x) = vals.iter_mut().find(|x| x.v == v) {
                x.en = l;
            }
        }
    }
    let arranque = match b.get("arranque") {
        None => None,
        Some("bandera") => Some(Arranque::Bandera),
        Some(k) if palabra_ok(k) && !k.contains([':', '=']) => Some(Arranque::Clave(k.to_string())),
        Some(_) => return Err("«arranque» no válido".into()),
    };
    if arranque.is_some() {
        if key.is_none() || palabra || b.get("aplicar").is_some() {
            return Err("«arranque» va en la «clave» de una línea de órdenes".into());
        }
        if !matches!(tipo, Tipo::Elegir | Tipo::SiNo) {
            return Err("«arranque» es para «elegir» o «si-no»".into());
        }
        // A ':' would add an item of its own to the command line.
        if vals.iter().any(|x| x.v.contains(':')) {
            return Err("un valor de «arranque» no puede llevar «:»".into());
        }
    }
    match &mut tipo {
        Tipo::Elegir if vals.is_empty() => return Err("«elegir» sin «valores»".into()),
        Tipo::SiNo if vals.len() != 2 => return Err("«si-no» lleva dos valores: sí y no".into()),
        Tipo::Numero { min, max, paso } => {
            let (a, z) = b
                .get("rango")
                .and_then(rango)
                .ok_or("«numero» sin «rango = a..b»")?;
            let p = b.get("paso").map(|p| p.parse::<i64>()).unwrap_or(Ok(1));
            let p = p.ok().filter(|p| *p > 0).ok_or("«paso» no válido")?;
            (*min, *max, *paso) = (a, z, p);
            vals.clear();
        }
        Tipo::Abrir if b.get("orden").is_none() => return Err("«abrir» sin «orden»".into()),
        _ => {}
    }
    let c = Clave {
        es: b.get("es").unwrap_or(&id).to_string(),
        en: b.get("en").unwrap_or("").to_string(),
        ayuda_es: b.get("ayuda").unwrap_or("").to_string(),
        ayuda_en: b.get("ayuda.en").unwrap_or("").to_string(),
        defecto: b.get("defecto").map(str::to_string),
        aplicar: b.get("aplicar").map(str::to_string),
        leer: b.get("leer").map(str::to_string),
        orden: b.get("orden").map(str::to_string),
        reabrir: matches!(b.get("reabrir"), Some("si" | "sí" | "yes")),
        reinicio: matches!(b.get("reinicio"), Some("si" | "sí" | "yes")),
        palabra,
        montar,
        arranque,
        id,
        grupo,
        tipo,
        fichero,
        clave: key,
        valores: vals,
    };
    if let Some(d) = &c.defecto {
        if !c.admite(d) {
            return Err(format!("«defecto = {d}» no es uno de sus valores"));
        }
    }
    Ok(c)
}

impl Catalogo {
    /// Adds one file's blocks. `origen` names it in the warnings.
    pub fn lee_texto(&mut self, text: &str, origen: &str) {
        for b in bloques(text, origen, &mut self.avisos) {
            if let Some(d) = b.cabecera.strip_prefix("driver:") {
                match crate::dispositivos::Driver::de_campos(d.trim(), |k| b.get(k)) {
                    Ok(x) if self.drivers.iter().any(|y| y.id == x.id) => self.avisos.push(format!(
                        "{}: [{}] repetido, se queda el primero",
                        b.donde, b.cabecera
                    )),
                    Ok(x) => self.drivers.push(x),
                    Err(why) => self
                        .avisos
                        .push(format!("{}: [{}] {why}", b.donde, b.cabecera)),
                }
                continue;
            }
            if let Some(g) = b.cabecera.strip_prefix("grupo:") {
                let g = g.trim();
                let grupo = Grupo {
                    id: g.to_string(),
                    es: b.get("es").unwrap_or(g).to_string(),
                    en: b.get("en").unwrap_or("").to_string(),
                };
                match self.grupos.iter_mut().find(|x| x.id == g) {
                    Some(x) => *x = grupo,
                    None => self.grupos.push(grupo),
                }
                continue;
            }
            match clave(&b) {
                Ok(c) if self.claves.iter().any(|x| x.id == c.id) => self.avisos.push(format!(
                    "{}: [{}] repetida, se queda la primera",
                    b.donde, c.id
                )),
                Ok(c) => {
                    if !self.grupos.iter().any(|g| g.id == c.grupo) {
                        self.grupos.push(Grupo {
                            id: c.grupo.clone(),
                            es: c.grupo.clone(),
                            en: String::new(),
                        });
                    }
                    self.claves.push(c);
                }
                Err(why) => self
                    .avisos
                    .push(format!("{}: [{}] {why}", b.donde, b.cabecera)),
            }
        }
    }

    /// Every `*.claves` in `dirs`, files in name order across all of them.
    pub fn carga(dirs: &[&Path]) -> Self {
        let mut files: Vec<PathBuf> = dirs
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flat_map(|r| r.flatten().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "claves"))
            .collect();
        files.sort_by_key(|p| p.file_name().map(|n| n.to_owned()));
        let mut cat = Catalogo::default();
        for f in files {
            match std::fs::read_to_string(&f) {
                Ok(t) => cat.lee_texto(&t, &f.display().to_string()),
                Err(e) => cat.avisos.push(format!("{}: {e}", f.display())),
            }
        }
        if cat.claves.is_empty() {
            cat = Catalogo::de_serie();
            cat.avisos
                .push("no hay claves.d instaladas: se usan las de serie".into());
        }
        // A group nobody put a setting in is not a page.
        let used: Vec<String> = cat.claves.iter().map(|c| c.grupo.clone()).collect();
        cat.grupos.retain(|g| used.contains(&g.id));
        cat
    }

    /// [`DE_SERIE`], parsed.
    pub fn de_serie() -> Self {
        let mut c = Catalogo::default();
        for (name, text) in DE_SERIE {
            c.lee_texto(text, name);
        }
        c
    }

    /// The settings of group `g`, as indices into `claves`.
    pub fn del_grupo(&self, g: usize) -> Vec<usize> {
        let Some(gr) = self.grupos.get(g) else {
            return Vec::new();
        };
        (0..self.claves.len())
            .filter(|&i| self.claves[i].grupo == gr.id)
            .collect()
    }

    /// Settings whose name (either language), id or file matches every word
    /// of `q`, case-blind.
    pub fn busca(&self, q: &str) -> Vec<usize> {
        let words: Vec<String> = q.split_whitespace().map(minus).collect();
        (0..self.claves.len())
            .filter(|&i| {
                let c = &self.claves[i];
                let hay = minus(&format!(
                    "{} {} {} {} {}",
                    c.es,
                    c.en,
                    c.id,
                    c.origen(),
                    c.ayuda_es
                ));
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect()
    }
}

/// Lower case with the Spanish accents folded, so `idioma` finds `Idioma`
/// and `aplicacion` finds `aplicación`.
pub fn minus(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' | 'Á' => 'a',
            'é' | 'É' => 'e',
            'í' | 'Í' => 'i',
            'ó' | 'Ó' => 'o',
            'ú' | 'Ú' | 'ü' | 'Ü' => 'u',
            'ñ' | 'Ñ' => 'n',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

// ── the files ────────────────────────────────────────────────────────────────

/// `key = value` from a settings file: the last spelling wins nothing, the
/// FIRST line does, which is what the shell readers (`awk ... exit`) do.
pub fn lee_clave(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim_end_matches('\r').trim();
        if l.starts_with('#') {
            return None;
        }
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// `key = value` read the way rboot reads its file: the LAST line wins.
pub fn lee_ultima(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .rfind(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
}

/// `text` with `key=value` in place of the LAST `key=` line (rboot's), or
/// added at the end.
pub fn pon_ultima(text: &str, key: &str, value: &str) -> String {
    let is_it = |l: &str| {
        let l = l.trim();
        !l.starts_with('#') && l.split_once('=').is_some_and(|(k, _)| k.trim() == key)
    };
    let lines: Vec<&str> = text.lines().collect();
    let Some(last) = lines.iter().rposition(|l| is_it(l)) else {
        return pon_clave(text, key, value);
    };
    let mut out = String::with_capacity(text.len() + value.len());
    for (i, l) in lines.iter().enumerate() {
        if i == last {
            out.push_str(&format!("{key}={value}\n"));
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// The items of a `:`-separated command line, as `(key, value)`.
fn items(cmdline: &str) -> impl Iterator<Item = (&str, &str)> {
    cmdline.split(':').map(|p| match p.split_once('=') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => (p.trim(), ""),
    })
}

/// What `a` finds in `cmdline`: the first `KEY=`'s value, or the first of
/// `valores` present as a token. `None` when it is not there.
pub fn lee_arranque(cmdline: &str, a: &Arranque, valores: &[Valor]) -> Option<String> {
    match a {
        Arranque::Clave(k) => items(cmdline)
            .find(|(x, _)| x.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.to_string()),
        Arranque::Bandera => cmdline
            .split(':')
            .map(str::trim)
            .find(|t| valores.iter().any(|x| x.v != "auto" && x.v == *t))
            .map(str::to_string),
    }
}

/// `cmdline` with `a` set to `v`: the item takes the place of its first
/// spelling (or goes at the end), later spellings go, and `auto` removes
/// it. Every other item is kept as it was, in its order.
pub fn pon_arranque(cmdline: &str, a: &Arranque, valores: &[Valor], v: &str) -> String {
    let mine = |p: &str| match a {
        Arranque::Clave(k) => items(p).next().is_some_and(|(x, _)| x.eq_ignore_ascii_case(k)),
        Arranque::Bandera => valores.iter().any(|x| x.v != "auto" && x.v == p.trim()),
    };
    let new = match a {
        _ if v == "auto" => None,
        Arranque::Clave(k) => Some(format!("{k}={v}")),
        Arranque::Bandera => Some(v.to_string()),
    };
    let mut out: Vec<String> = Vec::new();
    let mut placed = false;
    for p in cmdline.split(':').filter(|p| !p.trim().is_empty()) {
        if mine(p) {
            if !placed {
                out.extend(new.clone());
                placed = true;
            }
        } else {
            out.push(p.to_string());
        }
    }
    if !placed {
        out.extend(new);
    }
    out.join(":")
}

/// `text` with `key=value` in place of the first `key=` line, or added at
/// the end. Every other line, comments included, is left as it was.
pub fn pon_clave(text: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 2);
    let mut done = false;
    for l in text.lines() {
        let is_it = !done
            && !l.trim_start().starts_with('#')
            && l.split_once('=').is_some_and(|(k, _)| k.trim() == key);
        if is_it {
            out.push_str(&format!("{key}={value}\n"));
            done = true;
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !done {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

fn sh(cmd: &str) -> std::io::Result<std::process::Output> {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .output()
}

/// The value in force: `leer`'s first line, else the file, else `defecto`.
pub fn valor_actual(c: &Clave) -> String {
    if let Some(m) = &c.montar {
        if let Err(e) = asegura_montaje(m) {
            eprintln!("lunarpanel: {e}");
        }
    }
    valor_leido(c)
}

/// The volume `c` lives on when it is not mounted now: what
/// [`valor_actual`] would wait for.
pub fn sin_montar(c: &Clave) -> Option<&Path> {
    let m = c.montar.as_deref()?;
    let mounted = std::fs::read_to_string("/proc/mounts").is_ok_and(|t| montado_en(&t, m));
    (!mounted).then_some(m)
}

/// [`valor_actual`] without mounting anything: what the file says if its
/// volume is there, else the default. It never waits.
pub fn valor_leido(c: &Clave) -> String {
    if let Some(cmd) = &c.leer {
        if let Ok(o) = sh(cmd) {
            let out = String::from_utf8_lossy(&o.stdout);
            let line = out.lines().next().unwrap_or("").trim();
            // Only an answer that IS a value counts; a usage line or an
            // error printed to stdout falls through to the file.
            let known = match c.tipo {
                Tipo::Numero { .. } => line.parse::<i64>().is_ok(),
                _ => c.valores.iter().any(|x| x.v.eq_ignore_ascii_case(line)),
            };
            if o.status.success() && known {
                return c.normaliza(Some(line));
            }
        }
    }
    let text = c.fichero.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
    let raw = match (&text, &c.clave) {
        (Some(t), Some(k)) if c.arranque.is_some() => {
            let line = lee_ultima(t, k).unwrap_or_default();
            lee_arranque(&line, c.arranque.as_ref().unwrap(), &c.valores)
        }
        (Some(t), _) if c.palabra => lee_palabra(t),
        (Some(t), Some(k)) => lee_clave(t, k),
        _ => None,
    };
    c.normaliza(raw.as_deref())
}

/// The first word of a one-word settings file, comments skipped.
pub fn lee_palabra(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|l| l.split_whitespace().next())
        .map(str::to_string)
}

/// Whether `/proc/mounts` (as `text`) has something mounted at `at`.
pub fn montado_en(text: &str, at: &Path) -> bool {
    let at = at.to_string_lossy();
    let at = at.trim_end_matches('/');
    text.lines().any(|l| l.split_whitespace().nth(1) == Some(at))
}

/// What mounts fstab's FAT, btrfs, EclipseFS and NFS lines (the
/// `fs-mounts` service); it skips what is already mounted.
const FS_MOUNTS: &str = "/usr/local/bin/eclipse-fs-mounts";

/// `at` mounted, through fstab when it is not yet: the same script the boot
/// runs, then up to 6 s for the mount to show in /proc/mounts (the script
/// leaves its servers starting in the background).
///
/// Tried once per volume and process: every boot setting lives on
/// `/boot/efi`, and a volume fstab cannot mount cost 6 s for each of them,
/// a minute before the panel showed up.
pub fn asegura_montaje(at: &Path) -> Result<(), String> {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static PROBADOS: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    let now = || {
        std::fs::read_to_string("/proc/mounts")
            .map(|t| montado_en(&t, at))
            .unwrap_or(false)
    };
    if now() {
        return Ok(());
    }
    // Held through the attempt: a second caller (the panel's APLICAR while
    // its reader thread mounts) waits for the first one's answer.
    let mut probados = PROBADOS.lock().unwrap_or_else(|e| e.into_inner());
    if now() {
        return Ok(());
    }
    if !probados.get_or_insert_with(HashSet::new).insert(at.to_path_buf()) {
        return Err(format!("{} no está montada (fstab)", at.display()));
    }
    let _ = std::process::Command::new(FS_MOUNTS)
        .stdin(std::process::Stdio::null())
        .status();
    for _ in 0..60 {
        if now() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("{} no está montada (fstab)", at.display()))
}

/// Sets `c` to `v`: through `aplicar` when the block has one, else by
/// writing the file (atomically: a sibling temp file renamed over it).
/// `v` must be one the block admits; anything else is refused unwritten.
pub fn aplica(c: &Clave, v: &str) -> Result<(), String> {
    if !c.admite(v) || !palabra_ok(v) {
        return Err(format!("«{v}» no es un valor de {}", c.id));
    }
    if let Some(cmd) = &c.aplicar {
        let line = cmd.replace("{}", &format!("'{v}'"));
        let o = sh(&line).map_err(|e| format!("{line}: {e}"))?;
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().next().unwrap_or("").trim();
            return Err(format!(
                "{line}: {}",
                if first.is_empty() { "falló" } else { first }
            ));
        }
        return Ok(());
    }
    let Some(f) = &c.fichero else {
        return Err(format!("{} no dice dónde escribir", c.id));
    };
    if let Some(m) = &c.montar {
        asegura_montaje(m)?;
    }
    // A file on a mounted volume is edited, never made: an empty one in its
    // place would be an rboot.conf with nothing but this key.
    let old = match std::fs::read_to_string(f) {
        Ok(t) => t,
        Err(e) if c.montar.is_some() || c.arranque.is_some() => {
            return Err(format!("{}: {e}", f.display()))
        }
        Err(_) => String::new(),
    };
    let new = match &c.clave {
        Some(k) if c.arranque.is_some() => {
            let line = lee_ultima(&old, k).unwrap_or_default();
            let line = pon_arranque(&line, c.arranque.as_ref().unwrap(), &c.valores, v);
            pon_ultima(&old, k, &line)
        }
        _ if c.palabra => format!("{v}\n"),
        Some(k) => pon_clave(&old, k, v),
        None => return Err(format!("{} no dice qué clave escribir", c.id)),
    };
    escribe(f, &new)
}

/// `f` holds `new`: through a sibling temp file renamed over it, or written
/// in place where the filesystem cannot rename over a file.
fn escribe(f: &Path, new: &str) -> Result<(), String> {
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = f.with_extension(format!("lunarpanel.{}", std::process::id()));
    let atomic = std::fs::write(&tmp, new).and_then(|()| std::fs::rename(&tmp, f));
    if atomic.is_ok() {
        return Ok(());
    }
    // A filesystem that cannot rename over a file (the ESP's FAT server may
    // not): the file is rewritten in place instead.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(f, new).map_err(|e| format!("{}: {e}", f.display()))
}

/// `c`'s file changed by `cambia`, which is handed what is there: for an
/// edit no single value spells (a driver added to `udriver=`). The volume
/// is mounted first and the file must be there: it is edited, never made.
pub fn edita(c: &Clave, cambia: impl FnOnce(&str) -> String) -> Result<(), String> {
    let Some(f) = &c.fichero else {
        return Err(format!("{} no dice dónde escribir", c.id));
    };
    if let Some(m) = &c.montar {
        asegura_montaje(m)?;
    }
    let old = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
    escribe(f, &cambia(&old))
}

/// Starts an `abrir`'s command and does not wait for it.
pub fn abre(c: &Clave) -> Result<(), String> {
    let Some(cmd) = &c.orden else {
        return Err(format!("{} no tiene «orden»", c.id));
    };
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("{cmd}: {e}"))
}

#[cfg(test)]
#[path = "claves_tests.rs"]
mod tests;
