//! Which look the desktop is wearing: KDE's Breeze Dark, or Eclipse's own
//! violet. Persisted by `eclipse-look` in `/etc/eclipse/look` (and applied at
//! boot by eclipse-init), so every client agrees without an IPC bus.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// Windows 11 dark: one bottom taskbar with its buttons CENTRED,
    /// `#202020` ground, `#0078d4` accent.
    Win11,
    /// KDE Breeze Dark: one bottom panel, grey ground, `#3daee9` accent.
    Kde,
    /// Eclipse's own: two bars, blue-black ground, blue accent. The default,
    /// and what the image ships.
    Eclipse,
}

impl Look {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "win11" | "Win11" | "windows" | "windows11" => Some(Look::Win11),
            "kde" | "KDE" | "breeze" | "Breeze" => Some(Look::Kde),
            "eclipse" | "Eclipse" => Some(Look::Eclipse),
            _ => None,
        }
    }

    /// `$ECLIPSE_LOOK` (a launcher override) wins, then `/etc/eclipse/look`,
    /// else Eclipse's own look — which is what the image ships.
    pub fn current() -> Self {
        if let Some(l) = std::env::var("ECLIPSE_LOOK").ok().and_then(|v| Self::from_name(&v)) {
            return l;
        }
        if let Some(l) = file_look() {
            return l;
        }
        Look::Eclipse
    }
}

/// First `look=` line of `/etc/eclipse/look`, comments skipped.
fn file_look() -> Option<Look> {
    let text = std::fs::read_to_string("/etc/eclipse/look").ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("look=") {
            return Look::from_name(v);
        }
    }
    None
}
