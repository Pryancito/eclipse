//! `rc.xml` — formato de configuración de labwc, port a Eclipse OS.
//!
//! labwc lee `~/.config/labwc/rc.xml`. En Eclipse usamos
//! `eclipsefs:/etc/labwc/rc.xml` y, si falla, valores por defecto en código.
//!
//! Solo soportamos un subconjunto pragmático del XML — labwc completo tiene
//! muchísimas opciones, pero estas son las "core" que usa el 95% de configs:
//!
//! ```xml
//! <labwc_config>
//!   <core>
//!     <decoration>server</decoration>          <!-- server | client -->
//!     <gap>4</gap>
//!     <adaptiveSync>no</adaptiveSync>
//!     <reuseOutputMode>no</reuseOutputMode>
//!   </core>
//!   <theme>
//!     <name>Adwaita</name>
//!     <cornerRadius>6</cornerRadius>
//!     <font place="ActiveWindow"  name="sans" size="10" slant="normal" weight="bold"/>
//!     <font place="MenuItem"      name="sans" size="10"/>
//!   </theme>
//!   <focus>
//!     <followMouse>yes</followMouse>
//!     <raiseOnFocus>yes</raiseOnFocus>
//!   </focus>
//!   <keyboard>
//!     <keybind key="W-q">       <action name="Close"/></keybind>
//!     <keybind key="W-Tab">     <action name="NextWindow"/></keybind>
//!     <keybind key="W-Return">  <action name="Execute" command="terminal"/></keybind>
//!   </keyboard>
//!   <mouse>
//!     <context name="Root">
//!       <mousebind button="Right" action="Press"><action name="ShowMenu" menu="root-menu"/></mousebind>
//!     </context>
//!   </mouse>
//!   <windowRules>
//!     <windowRule identifier="firefox"><action name="Maximize"/></windowRule>
//!   </windowRules>
//!   <autostart>
//!     <command>lunas-panel &amp;</command>
//!   </autostart>
//! </labwc_config>
//! ```

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::actions::Action;
use crate::key::Keybind;

/// Modelo de focus, igual que labwc `<focus>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusModel {
    #[default]
    ClickToFocus,
    SloppyFocus,
    FollowMouse,
}

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub decoration: DecorationMode,
    pub gap: i32,
    pub adaptive_sync: bool,
    pub reuse_output_mode: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecorationMode {
    #[default]
    Server,
    Client,
}

#[derive(Clone, Debug)]
pub struct ThemeRef {
    /// Nombre de la carpeta de tema bajo `~/.local/share/themes/<name>/openbox-3/`.
    pub name: String,
    pub corner_radius: i32,
}

#[derive(Clone, Debug)]
pub struct FocusConfig {
    pub model: FocusModel,
    pub raise_on_focus: bool,
}

#[derive(Clone, Debug, Default)]
pub struct WindowRule {
    /// Coincide con `app_id` (Wayland) o `class` (X11).
    pub identifier: String,
    /// Lista de actions que se aplican al mapearse.
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug)]
pub struct LabwcConfig {
    pub core: CoreConfig,
    pub theme: ThemeRef,
    pub focus: FocusConfig,
    pub keybinds: Vec<Keybind>,
    pub mousebinds: Vec<MouseBind>,
    pub window_rules: Vec<WindowRule>,
    pub autostart: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MouseBind {
    pub context: MouseContext,
    pub button: MouseButton,
    pub event: MouseEvent,
    pub action: Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseContext { Root, Title, Frame, Client }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton  { Left, Middle, Right }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent   { Press, Release, DoubleClick, Drag }

impl Default for LabwcConfig {
    fn default() -> Self {
        Self {
            core: CoreConfig {
                decoration: DecorationMode::Server,
                gap: 4,
                adaptive_sync: false,
                reuse_output_mode: false,
            },
            theme: ThemeRef { name: "Default".to_string(), corner_radius: 6 },
            focus: FocusConfig { model: FocusModel::ClickToFocus, raise_on_focus: true },
            keybinds: crate::key::default_keybinds(),
            mousebinds: default_mousebinds(),
            window_rules: Vec::new(),
            autostart: Vec::new(),
        }
    }
}

fn default_mousebinds() -> Vec<MouseBind> {
    use crate::actions::Action;
    alloc::vec![
        MouseBind {
            context: MouseContext::Root,
            button: MouseButton::Right, event: MouseEvent::Press,
            action: Action::ShowMenu("root-menu".to_string()),
        },
        MouseBind {
            context: MouseContext::Title,
            button: MouseButton::Left, event: MouseEvent::Drag,
            action: Action::Move,
        },
        MouseBind {
            context: MouseContext::Title,
            button: MouseButton::Left, event: MouseEvent::DoubleClick,
            action: Action::ToggleMaximize,
        },
        MouseBind {
            context: MouseContext::Frame,
            button: MouseButton::Left, event: MouseEvent::Drag,
            action: Action::Resize,
        },
    ]
}

impl LabwcConfig {
    /// Carga `rc.xml` desde la ruta canónica de Eclipse OS, con fallback a defaults.
    pub fn load() -> Self {
        let paths = [
            "/etc/labwc/rc.xml",
            "/usr/share/labwc/rc.xml",
        ];
        for p in &paths {
            if let Some(data) = read_file(p) {
                if let Some(cfg) = parse_rcxml(&data) {
                    return cfg;
                }
            }
        }
        Self::default()
    }

    /// Recarga (acción `Reconfigure` de labwc — `kill -SIGHUP labwc`).
    pub fn reload(&mut self) {
        *self = Self::load();
    }
}

/// Lectura de fichero usando libc Eclipse (`open`/`read`/`close`).
fn read_file(path: &str) -> Option<Vec<u8>> {
    use libc::{open, read, close, O_RDONLY};
    let mut cstr = path.as_bytes().to_vec();
    cstr.push(0);
    let fd = unsafe { open(cstr.as_ptr() as *const core::ffi::c_char, O_RDONLY, 0) };
    if fd < 0 { return None; }
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        let n = unsafe { read(fd, tmp.as_mut_ptr() as *mut core::ffi::c_void, tmp.len()) };
        if n <= 0 { break; }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    unsafe { close(fd); }
    Some(buf)
}

/// Parser XML *muy* mínimo y específico para `rc.xml` (no_std friendly,
/// sin dependencias externas como `quick-xml` que arrastran rustix/std).
///
/// El parser es tolerante: ignora etiquetas desconocidas y valores mal formados;
/// devuelve `None` solo si no ve la raíz `<labwc_config>`.
pub fn parse_rcxml(bytes: &[u8]) -> Option<LabwcConfig> {
    let s = core::str::from_utf8(bytes).ok()?;
    if !s.contains("<labwc_config") { return None; }

    let mut cfg = LabwcConfig::default();
    cfg.keybinds.clear();        // Si el usuario provee config, sustituimos defaults.
    cfg.mousebinds.clear();

    // <core><decoration>X</decoration></core>
    if let Some(v) = inner_text(s, "decoration") {
        cfg.core.decoration = match v.trim() {
            "client" => DecorationMode::Client,
            _        => DecorationMode::Server,
        };
    }
    if let Some(v) = inner_text(s, "gap") {
        cfg.core.gap = v.trim().parse().unwrap_or(4);
    }
    if let Some(v) = inner_text(s, "adaptiveSync") {
        cfg.core.adaptive_sync = v.trim().eq_ignore_ascii_case("yes");
    }

    // <theme><name>X</name><cornerRadius>R</cornerRadius></theme>
    if let Some(name) = inner_text(s, "name") {
        cfg.theme.name = name.trim().to_string();
    }
    if let Some(cr) = inner_text(s, "cornerRadius") {
        cfg.theme.corner_radius = cr.trim().parse().unwrap_or(6);
    }

    // <focus><followMouse>yes</followMouse><raiseOnFocus>no</raiseOnFocus></focus>
    if let Some(v) = inner_text(s, "followMouse") {
        if v.trim().eq_ignore_ascii_case("yes") {
            cfg.focus.model = FocusModel::FollowMouse;
        }
    }
    if let Some(v) = inner_text(s, "raiseOnFocus") {
        cfg.focus.raise_on_focus = v.trim().eq_ignore_ascii_case("yes");
    }

    // <keybind key="W-q"><action name="Close"/></keybind>
    for (key_attr, body) in iter_tags(s, "keybind") {
        if let (Some(k), Some(a)) = (attr(&key_attr, "key"), parse_action(&body)) {
            if let Some(kb) = crate::key::parse_keybind(&k, a) {
                cfg.keybinds.push(kb);
            }
        }
    }

    // <command>X</command>  (autostart)
    for (_, body) in iter_tags(s, "command") {
        cfg.autostart.push(body.trim().to_string());
    }

    // <windowRule identifier="X"><action ../></windowRule>
    for (attrs, body) in iter_tags(s, "windowRule") {
        if let Some(id) = attr(&attrs, "identifier") {
            let mut rule = WindowRule { identifier: id, actions: Vec::new() };
            for (a_attrs, _) in iter_tags(&body, "action") {
                if let Some(act) = action_from_attrs(&a_attrs) {
                    rule.actions.push(act);
                }
            }
            cfg.window_rules.push(rule);
        }
    }

    // Defaults si no se proveyeron.
    if cfg.keybinds.is_empty()   { cfg.keybinds = crate::key::default_keybinds(); }
    if cfg.mousebinds.is_empty() { cfg.mousebinds = default_mousebinds(); }

    Some(cfg)
}

// Re-exports públicos para que `menu.rs` reuse los mismos helpers.
pub fn iter_tags_pub(s: &str, tag: &str) -> Vec<(String, String)> { iter_tags(s, tag) }
pub fn attr_pub(attrs: &str, key: &str) -> Option<String> { attr(attrs, key) }
pub fn action_from_attrs_pub(attrs: &str) -> Option<crate::actions::Action> { action_from_attrs(attrs) }

// ── helpers de parsing XML ───────────────────────────────────────────────────
fn inner_text(s: &str, tag: &str) -> Option<String> {
    let open = alloc::format!("<{tag}");
    let close = alloc::format!("</{tag}>");
    let i = s.find(&open)?;
    let body_start = s[i..].find('>')? + i + 1;
    let j = s[body_start..].find(&close)? + body_start;
    Some(s[body_start..j].to_string())
}

fn iter_tags(s: &str, tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let open = alloc::format!("<{tag}");
    let close = alloc::format!("</{tag}>");
    let mut cursor = 0;
    while let Some(rel) = s[cursor..].find(&open) {
        let i = cursor + rel;
        let attr_end = match s[i..].find('>') { Some(e) => i + e, None => break };
        let attrs = s[i + open.len()..attr_end].to_string();
        let body_start = attr_end + 1;
        if s[i..attr_end].ends_with('/') {
            // self-closing
            out.push((attrs, String::new()));
            cursor = body_start;
            continue;
        }
        let body_end = match s[body_start..].find(&close) {
            Some(e) => body_start + e,
            None => break,
        };
        out.push((attrs, s[body_start..body_end].to_string()));
        cursor = body_end + close.len();
    }
    out
}

fn attr(attrs: &str, key: &str) -> Option<String> {
    let pat = alloc::format!("{key}=\"");
    let i = attrs.find(&pat)?;
    let start = i + pat.len();
    let end = start + attrs[start..].find('"')?;
    Some(attrs[start..end].to_string())
}

fn parse_action(body: &str) -> Option<Action> {
    let (attrs, _) = iter_tags(body, "action").into_iter().next()?;
    action_from_attrs(&attrs)
}

fn action_from_attrs(attrs: &str) -> Option<Action> {
    let name = attr(attrs, "name")?;
    Some(match name.as_str() {
        "Close"           => Action::Close,
        "Iconify"         => Action::Iconify,
        "ToggleMaximize"  => Action::ToggleMaximize,
        "Maximize"        => Action::Maximize,
        "Move"            => Action::Move,
        "Resize"          => Action::Resize,
        "NextWindow"      => Action::NextWindow,
        "PreviousWindow"  => Action::PreviousWindow,
        "Reconfigure"     => Action::Reconfigure,
        "Exit"            => Action::Exit,
        "ShowMenu"        => Action::ShowMenu(attr(attrs, "menu").unwrap_or_else(|| "root-menu".into())),
        "Execute"         => Action::Execute(attr(attrs, "command").unwrap_or_default()),
        "GoToDesktop"     => Action::GoToDesktop(attr(attrs, "to").and_then(|v| v.parse().ok()).unwrap_or(0)),
        _ => return None,
    })
}
