//! Which look the desktop is wearing: KDE's Breeze Dark, or Eclipse's own
//! violet. Persisted by `eclipse-look` in `/etc/eclipse/look` (and applied at
//! boot by eclipse-init), so every client agrees without an IPC bus.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// KDE Breeze Dark: one bottom panel, grey ground, `#3daee9` accent.
    Kde,
    /// Eclipse's original: two bars, blue-black ground, blue accent.
    Eclipse,
}

impl Look {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "kde" | "KDE" | "breeze" | "Breeze" => Some(Look::Kde),
            "eclipse" | "Eclipse" => Some(Look::Eclipse),
            _ => None,
        }
    }

    /// `$ECLIPSE_LOOK` (a launcher override) wins, then `/etc/eclipse/look`,
    /// else KDE — which is what the image ships.
    pub fn current() -> Self {
        if let Some(l) = std::env::var("ECLIPSE_LOOK").ok().and_then(|v| Self::from_name(&v)) {
            return l;
        }
        if let Some(l) = file_look() {
            return l;
        }
        Look::Kde
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
