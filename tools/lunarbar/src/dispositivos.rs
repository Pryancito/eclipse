//! The machine's devices and who drives them: what the control panel's
//! "Dispositivos y drivers" page shows.
//!
//! Three sources, all read, none guessed:
//! - **the bus**: `/sys/bus/pci/devices/<bdf>/{vendor,device,class}`, the
//!   same scan the kernel matched its drivers against;
//! - **the names**: `pci.ids`, the database `lspci` reads (`hwdata-pci`),
//!   for the maker, the model and the class;
//! - **the drivers**: the `[driver:ID]` blocks of claves.d (data, like
//!   every other setting), `/proc/udrivers` (what the kernel left to
//!   userspace, `kind@N`) and eclipse-init's state file (whether the
//!   service for that device is running).
//!
//! A device no block takes is "sin driver". One a block gives a userspace
//! kind can be handed to it at the next boot by naming it in the kernel's
//! `udriver=` list ([`con_udriver`]); one the kernel drives can be given
//! back the same way.
//!
//! ```text
//! [driver:nvme]
//! es      = udrv-nvme
//! clase   = 0108          # hex prefix of base class, subclass, prog-if
//! nucleo  = si            # the kernel drives it too (a root on NVMe)
//! udriver = nvme          # the kernel's reservation kind
//! ```
//!
//! `ids = 8086:100e 8086:100f` and `fabricante = 10de` match by id; a
//! block names at least one of `ids`, `fabricante` or `clase`. Blocks are
//! tried in order and the first that matches wins, so specific ones go
//! first.

use std::collections::HashMap;
use std::path::Path;

use crate::claves::{self, Arranque, Clave};
use crate::sysmon::Svc;

/// Where the kernel lists the PCI functions.
pub const BUS: &str = "/sys/bus/pci/devices";
/// What the kernel left to drivers in userspace, one `kind@N` a line.
pub const UDRIVERS: &str = "/proc/udrivers";
/// Where `pci.ids` is, first found wins (Alpine's `hwdata-pci`, then the
/// pciutils spelling).
pub const PCI_IDS: [&str; 2] = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"];

/// One PCI function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pci {
    /// `0000:01:00.0`.
    pub bdf: String,
    pub vendor: u16,
    pub device: u16,
    /// Base class, subclass and programming interface: `0x010802`.
    pub class: u32,
}

impl Pci {
    /// `10de:2684`.
    pub fn ids(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor, self.device)
    }

    /// `01:00.0`: the bdf without the domain when it is the first one.
    pub fn donde(&self) -> &str {
        self.bdf.strip_prefix("0000:").unwrap_or(&self.bdf)
    }
}

/// Every PCI function under `dir`, in bdf order (the kernel's probe order).
pub fn bus_en(dir: &Path) -> Vec<Pci> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let num = |p: std::path::PathBuf| -> Option<u32> {
        let t = std::fs::read_to_string(p).ok()?;
        u32::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok()
    };
    let mut out: Vec<Pci> = rd
        .flatten()
        .filter_map(|e| {
            let at = e.path();
            Some(Pci {
                bdf: e.file_name().to_string_lossy().into_owned(),
                vendor: num(at.join("vendor"))? as u16,
                device: num(at.join("device"))? as u16,
                class: num(at.join("class"))?,
            })
        })
        .collect();
    out.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    out
}

// ── pci.ids ──────────────────────────────────────────────────────────────────

/// A subclass: its name and its programming interfaces' names.
type Subclase = (String, HashMap<u8, String>);

/// The names `lspci` prints, from `pci.ids`.
#[derive(Default, Debug)]
pub struct Nombres {
    vendors: HashMap<u16, (String, HashMap<u16, String>)>,
    /// Base class → (name, subclass → its names).
    classes: HashMap<u8, (String, HashMap<u8, Subclase>)>,
}

fn hex<T: TryFrom<u32>>(s: &str, digits: usize) -> Option<T> {
    if s.len() != digits {
        return None;
    }
    T::try_from(u32::from_str_radix(s, 16).ok()?).ok()
}

/// `"10de  NVIDIA Corporation"` → `("10de", "NVIDIA Corporation")`.
fn id_nombre(l: &str) -> Option<(&str, &str)> {
    let (id, name) = l.split_once(' ')?;
    Some((id, name.trim()))
}

impl Nombres {
    /// The database in `text` (`pci.ids`'s own format). Lines it does not
    /// understand are skipped: it is a name table, not a setting.
    pub fn lee(text: &str) -> Self {
        let mut n = Nombres::default();
        let mut vendor: Option<u16> = None;
        let mut class: Option<u8> = None;
        let mut sub: Option<u8> = None;
        let mut in_classes = false;
        for l in text.lines() {
            if l.starts_with('#') || l.trim().is_empty() {
                continue;
            }
            if let Some(rest) = l.strip_prefix("C ") {
                in_classes = true;
                class = id_nombre(rest).and_then(|(id, name)| {
                    let c = hex::<u8>(id, 2)?;
                    n.classes.insert(c, (name.to_string(), HashMap::new()));
                    Some(c)
                });
                sub = None;
                continue;
            }
            let tabs = l.len() - l.trim_start_matches('\t').len();
            let body = &l[tabs..];
            if in_classes {
                let (Some((id, name)), Some(c)) = (id_nombre(body), class) else {
                    continue;
                };
                let Some(entry) = n.classes.get_mut(&c) else {
                    continue;
                };
                match tabs {
                    1 => {
                        sub = hex::<u8>(id, 2);
                        if let Some(s) = sub {
                            entry.1.insert(s, (name.to_string(), HashMap::new()));
                        }
                    }
                    2 => {
                        if let (Some(s), Some(p)) = (sub, hex::<u8>(id, 2)) {
                            if let Some(e) = entry.1.get_mut(&s) {
                                e.1.insert(p, name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let Some((id, name)) = id_nombre(body) else {
                continue;
            };
            match tabs {
                0 => {
                    vendor = hex::<u16>(id, 4);
                    if let Some(v) = vendor {
                        n.vendors.insert(v, (name.to_string(), HashMap::new()));
                    }
                }
                1 => {
                    if let (Some(v), Some(d)) = (vendor, hex::<u16>(id, 4)) {
                        if let Some(e) = n.vendors.get_mut(&v) {
                            e.1.insert(d, name.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        n
    }

    /// The first `pci.ids` of [`PCI_IDS`] there is, else no names.
    pub fn carga() -> Self {
        PCI_IDS
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map(|t| Nombres::lee(&t))
            .unwrap_or_default()
    }

    pub fn fabricante(&self, vendor: u16) -> Option<&str> {
        self.vendors.get(&vendor).map(|v| v.0.as_str())
    }

    pub fn modelo(&self, vendor: u16, device: u16) -> Option<&str> {
        self.vendors.get(&vendor)?.1.get(&device).map(String::as_str)
    }

    /// The most precise name the class has: prog-if, else subclass, else
    /// base class.
    pub fn clase(&self, class: u32) -> Option<&str> {
        let (b, s, p) = ((class >> 16) as u8, (class >> 8) as u8, class as u8);
        let base = self.classes.get(&b)?;
        let Some(sub) = base.1.get(&s) else {
            return Some(&base.0);
        };
        Some(sub.1.get(&p).unwrap_or(&sub.0))
    }

    pub fn vacio(&self) -> bool {
        self.vendors.is_empty()
    }
}

// ── the drivers (data) ───────────────────────────────────────────────────────

/// One `[driver:ID]` block.
#[derive(Clone, Debug, PartialEq)]
pub struct Driver {
    pub id: String,
    /// What the page calls it (`udrv-nvme`, `nvidia`).
    pub es: String,
    pub ids: Vec<(u16, u16)>,
    pub fabricante: Option<u16>,
    /// The hex prefix of the class: `(value, digits)`.
    pub clase: Option<(u32, u32)>,
    /// Whether the kernel drives the device when it is not handed over.
    pub nucleo: bool,
    /// The kernel's reservation kind (`nvme`, `xhci`): a driver in
    /// userspace can have it.
    pub udriver: Option<String>,
    /// `necesita = no`: the device needs no driver of its own (a bridge,
    /// the interrupt controller); the page says so instead of "sin driver".
    pub necesita: bool,
    /// `instalado = si`: a driver installed from the panel
    /// (`eclipse-udriver`), which it can also remove. It goes before the
    /// shipped blocks for its device.
    pub instalado: bool,
}

impl Driver {
    /// A block's fields as a driver; fail-closed like every block.
    pub fn de_campos<'a>(id: &str, get: impl Fn(&str) -> Option<&'a str>) -> Result<Driver, String> {
        if !claves::palabra_ok(id) {
            return Err("nombre de driver no válido".into());
        }
        let mut ids = Vec::new();
        for pair in get("ids").unwrap_or("").split_whitespace() {
            let (v, d) = pair.split_once(':').ok_or("«ids» lleva vvvv:dddd")?;
            match (hex::<u16>(v, 4), hex::<u16>(d, 4)) {
                (Some(v), Some(d)) => ids.push((v, d)),
                _ => return Err(format!("«ids»: {pair} no es vvvv:dddd")),
            }
        }
        let fabricante = match get("fabricante") {
            None => None,
            Some(v) => Some(hex::<u16>(v, 4).ok_or("«fabricante» lleva cuatro cifras hex")?),
        };
        let clase = match get("clase") {
            None => None,
            Some(c) if matches!(c.len(), 2 | 4 | 6) => Some((
                u32::from_str_radix(c, 16).map_err(|_| "«clase» no es hex")?,
                c.len() as u32,
            )),
            Some(_) => return Err("«clase» lleva 2, 4 o 6 cifras hex".into()),
        };
        if ids.is_empty() && fabricante.is_none() && clase.is_none() {
            return Err("falta «ids», «fabricante» o «clase»".into());
        }
        let udriver = get("udriver").map(str::to_string);
        if udriver.as_deref().is_some_and(|k| !claves::palabra_ok(k) || k.contains([':', '@', ','])) {
            return Err("«udriver» no válido".into());
        }
        Ok(Driver {
            id: id.to_string(),
            es: get("es").unwrap_or(id).to_string(),
            ids,
            fabricante,
            clase,
            nucleo: !matches!(get("nucleo"), Some("no")),
            necesita: !matches!(get("necesita"), Some("no")),
            instalado: matches!(get("instalado"), Some("si")),
            udriver,
        })
    }

    pub fn casa(&self, p: &Pci) -> bool {
        if !self.ids.is_empty() {
            return self.ids.contains(&(p.vendor, p.device));
        }
        self.fabricante.is_none_or(|v| v == p.vendor)
            && self
                .clase
                .is_none_or(|(c, digits)| p.class >> (24 - 4 * digits) == c)
    }
}

// ── the page ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Estado {
    /// Handed to userspace and its service is running.
    Usuario,
    /// Handed to userspace and no service is driving it.
    Parado,
    /// The kernel drives it.
    Nucleo,
    /// A driver in userspace can have it; nothing drives it now.
    Libre,
    /// No block takes it.
    SinDriver,
    /// Its block says it needs no driver.
    Sistema,
}

/// One row of the page.
#[derive(Clone, Debug)]
pub struct Fila {
    pub pci: Pci,
    /// The model, else the class, else the ids.
    pub nombre: String,
    pub fabricante: Option<String>,
    pub clase: Option<String>,
    /// Index into the drivers.
    pub driver: Option<usize>,
    /// `nvme@0`: which of its kind it is, for `udriver=`.
    pub item: Option<String>,
    pub estado: Estado,
    /// Whether the boot's `udriver=` names it.
    pub pedido: bool,
}

impl Fila {
    /// What turning it over does: `Some(true)` hands it to userspace at the
    /// next boot, `Some(false)` gives it back to the kernel, `None` nothing
    /// to do.
    pub fn accion(&self) -> Option<bool> {
        self.item.as_ref()?;
        match self.estado {
            _ if self.pedido => Some(false),
            Estado::Nucleo | Estado::Libre => Some(true),
            _ => None,
        }
    }

    /// One line for the clipboard or a report.
    pub fn linea(&self) -> String {
        format!(
            "{} {} [{:06x}] {}{}",
            self.pci.donde(),
            self.pci.ids(),
            self.pci.class,
            self.fabricante.as_deref().map(|f| format!("{f} ")).unwrap_or_default(),
            self.nombre
        )
    }
}

/// The `udriver=` list of a command line, as items.
pub fn lista_udriver(cmdline: &str) -> Vec<String> {
    claves::lee_arranque(cmdline, &Arranque::Clave("udriver".into()), &[])
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `cmdline` with `item` added to (`on`) or taken out of its `udriver=`
/// list; the list goes when it is left empty.
pub fn con_udriver(cmdline: &str, item: &str, on: bool) -> String {
    let mut list = lista_udriver(cmdline);
    list.retain(|x| x != item);
    if on {
        list.push(item.to_string());
    }
    let v = if list.is_empty() { "auto".to_string() } else { list.join(",") };
    claves::pon_arranque(cmdline, &Arranque::Clave("udriver".into()), &[], &v)
}

/// The rows, from what was read. `udrivers` is `/proc/udrivers`, `svcs`
/// eclipse-init's services and `cmdline` the boot's line in rboot.conf.
pub fn calcula(
    bus: &[Pci],
    drivers: &[Driver],
    nombres: &Nombres,
    udrivers: &str,
    svcs: &[Svc],
    cmdline: &str,
) -> Vec<Fila> {
    let left: Vec<&str> = udrivers.lines().map(str::trim).collect();
    let pedidos = lista_udriver(cmdline);
    let mut cuenta: HashMap<String, usize> = HashMap::new();
    bus.iter()
        .map(|p| {
            let driver = drivers
                .iter()
                .position(|d| d.instalado && d.casa(p))
                .or_else(|| drivers.iter().position(|d| d.casa(p)));
            let item = driver.and_then(|i| drivers[i].udriver.as_ref()).map(|k| {
                let n = cuenta.entry(k.clone()).or_insert(0);
                *n += 1;
                format!("{k}@{}", *n - 1)
            });
            let estado = match (driver, &item) {
                (None, _) => Estado::SinDriver,
                (Some(i), _) if !drivers[i].necesita => Estado::Sistema,
                (Some(_), Some(it)) if left.contains(&it.as_str()) => {
                    let running = svcs
                        .iter()
                        .any(|s| s.udriver.as_deref() == Some(it) && s.state == "running");
                    if running {
                        Estado::Usuario
                    } else {
                        Estado::Parado
                    }
                }
                (Some(i), _) if drivers[i].nucleo => Estado::Nucleo,
                (Some(_), Some(_)) => Estado::Libre,
                (Some(_), None) => Estado::Nucleo,
            };
            let modelo = nombres.modelo(p.vendor, p.device).map(str::to_string);
            let clase = nombres.clase(p.class).map(str::to_string);
            Fila {
                nombre: modelo.or_else(|| clase.clone()).unwrap_or_else(|| p.ids()),
                fabricante: nombres.fabricante(p.vendor).map(str::to_string),
                clase,
                pedido: item.as_ref().is_some_and(|it| pedidos.contains(it)),
                driver,
                item,
                estado,
                pci: p.clone(),
            }
        })
        .collect()
}

/// The page read from this machine: `c` is the `dispositivos` block, which
/// says where the boot's command line is.
pub fn lee(c: &Clave, drivers: &[Driver], nombres: &Nombres) -> Vec<Fila> {
    let cmdline = c
        .fichero
        .as_ref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| claves::lee_ultima(&t, c.clave.as_deref().unwrap_or("cmdline")))
        .unwrap_or_default();
    let svcs = std::fs::read_to_string(crate::sysmon::SERVICES_FILE)
        .map(|t| crate::sysmon::parse_services(&t))
        .unwrap_or_default();
    calcula(
        &bus_en(Path::new(BUS)),
        drivers,
        nombres,
        &std::fs::read_to_string(UDRIVERS).unwrap_or_default(),
        &svcs,
        &cmdline,
    )
}

/// Hands `item` to userspace (`on`) or back to the kernel at the next boot,
/// in the `udriver=` of the command line `c` names.
pub fn cambia(c: &Clave, item: &str, on: bool) -> Result<(), String> {
    let ok = item
        .split_once('@')
        .is_some_and(|(k, n)| claves::palabra_ok(k) && !k.contains([':', ',']) && n.parse::<usize>().is_ok());
    if !ok {
        return Err(format!("«{item}» no es un dispositivo"));
    }
    let key = c.clave.clone().unwrap_or_else(|| "cmdline".into());
    claves::edita(c, |old| {
        let line = claves::lee_ultima(old, &key).unwrap_or_default();
        claves::pon_ultima(old, &key, &con_udriver(&line, item, on))
    })
}

// ── installing a driver ──────────────────────────────────────────────────────

/// The script that installs and removes a driver (xtask writes it).
pub const INSTALADOR: &str = "/usr/local/bin/eclipse-udriver";

/// The name a driver installed for this device takes: its file, its
/// service and its block are all called this.
pub fn servicio(p: &Pci) -> String {
    format!("udrv-{:04x}-{:04x}", p.vendor, p.device)
}

/// What the kernel calls the first device with this identity
/// (`udriver=pci-VVVV-DDDD@0`): the one an installed driver is for.
pub fn item_pci(p: &Pci) -> String {
    format!("pci-{:04x}-{:04x}@0", p.vendor, p.device)
}

/// Where the panel looks for a driver to install: the home directory and
/// its downloads.
pub fn dirs_candidatos(home: &Path) -> Vec<std::path::PathBuf> {
    vec![home.to_path_buf(), home.join("Descargas"), home.join("Downloads")]
}

/// Whether `path` is a driver file: its name ends in `.drv`.
pub fn es_drv(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("drv"))
}

/// The device a driver file says it is for, by its name: the last
/// `VVVV-DDDD` (or `VVVV:DDDD`, `VVVV_DDDD`) in it, as in
/// `mt7922-14c3-0616.drv` (the last, so a model number before the ids is
/// not taken for a vendor). `None` when the name holds no such pair.
pub fn ids_de_nombre(path: &Path) -> Option<(u16, u16)> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let b = name.as_bytes();
    let hex4 = |i: usize| {
        b.get(i..i + 4)
            .filter(|w| w.iter().all(u8::is_ascii_hexdigit))
            .and_then(|w| u16::from_str_radix(std::str::from_utf8(w).ok()?, 16).ok())
    };
    // A pair on its own: not the tail of a longer run of hex digits.
    let alone = |i: usize| i == 0 || !b[i - 1].is_ascii_hexdigit();
    let ends = |i: usize| b.get(i).is_none_or(|c| !c.is_ascii_hexdigit());
    (0..b.len()).rev().find_map(|i| {
        let ok = alone(i) && matches!(b.get(i + 4), Some(b'-' | b':' | b'_')) && ends(i + 9);
        ok.then(|| Some((hex4(i)?, hex4(i + 5)?))).flatten()
    })
}

/// The driver files (`*.drv`) in `dirs`, the ones whose name says they are
/// for `ids` first, then by name. A downloaded file has no execute bit;
/// the installer gives it one.
pub fn candidatos(dirs: &[std::path::PathBuf], ids: (u16, u16)) -> Vec<std::path::PathBuf> {
    let mut out: Vec<(bool, std::path::PathBuf)> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if es_drv(&p) && e.metadata().is_ok_and(|m| m.is_file()) {
                out.push((ids_de_nombre(&p) != Some(ids), p));
            }
        }
    }
    out.sort();
    out.dedup();
    out.into_iter().map(|(_, p)| p).collect()
}

/// Installs `fichero` as the driver of `p` (`instalar`) or removes the one
/// installed (`fichero` None), through [`INSTALADOR`]; then hands the
/// device to it at the next boot, or back to the kernel, in the command
/// line `c` names. A removal hands the device back first.
pub fn instala(c: &Clave, p: &Pci, fichero: Option<&Path>) -> Result<(), String> {
    instala_con(INSTALADOR, c, p, fichero)
}

/// [`instala`] with the script named, for the tests.
pub fn instala_con(script: &str, c: &Clave, p: &Pci, fichero: Option<&Path>) -> Result<(), String> {
    let v = format!("{:04x}", p.vendor);
    let d = format!("{:04x}", p.device);
    let item = item_pci(p);
    if fichero.is_none() {
        cambia(c, &item, false)?;
    }
    let mut cmd = std::process::Command::new(script);
    match fichero {
        Some(f) => cmd.arg("instalar").args([&v, &d]).arg(f),
        None => cmd.arg("quitar").args([&v, &d]),
    };
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{script}: {e}"))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(why.lines().last().unwrap_or("eclipse-udriver falló").to_string());
    }
    if fichero.is_some() {
        cambia(c, &item, true)?;
    }
    Ok(())
}

/// A plain-text report of the page, for whoever is asked for a driver.
pub fn informe(filas: &[Fila], drivers: &[Driver]) -> String {
    let mut out = String::from("# Eclipse OS: dispositivos PCI y sus drivers\n");
    for f in filas {
        let d = f.driver.and_then(|i| drivers.get(i)).map(|d| d.es.as_str()).unwrap_or("-");
        let st = match f.estado {
            Estado::Usuario => "usuario",
            Estado::Parado => "usuario (parado)",
            Estado::Nucleo => "nucleo",
            Estado::Libre => "libre",
            Estado::SinDriver => "SIN DRIVER",
            Estado::Sistema => "no necesita",
        };
        out.push_str(&format!("{}\tdriver={d}\t{st}\n", f.linea()));
    }
    out
}

#[cfg(test)]
#[path = "dispositivos_tests.rs"]
mod tests;
