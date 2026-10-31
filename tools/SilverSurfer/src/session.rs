//! Persistencia de pestañas entre arranques.
//!
//! Formato a propósito trivial (una línea JSON por campo) para no meter
//! `serde` en el crate host. El fichero vive en XDG data:
//! `$XDG_DATA_HOME/silversurfer/session.json` o `~/.local/share/…`.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedTab {
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub active: usize,
    pub tabs: Vec<SavedTab>,
}

impl Snapshot {
    pub fn encode(&self) -> String {
        let mut out = String::from("{\n");
        out.push_str(&format!("  \"active\": {},\n", self.active.min(self.tabs.len().saturating_sub(1))));
        out.push_str("  \"tabs\": [\n");
        for (i, tab) in self.tabs.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"url\": {},\n", json_string(&tab.url)));
            out.push_str(&format!("      \"title\": {}\n", json_string(&tab.title)));
            out.push_str("    }");
            if i + 1 < self.tabs.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    pub fn decode(text: &str) -> Option<Self> {
        let active = find_number(text, "\"active\"")?;
        let mut tabs = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            // Skip the root object: only objects that contain "url".
            let chunk = &rest[start..];
            let end = chunk.find('}')?;
            let obj = &chunk[..=end];
            if obj.contains("\"url\"") {
                let url = find_string(obj, "\"url\"")?;
                let title = find_string(obj, "\"title\"").unwrap_or_default();
                if !url.is_empty() {
                    tabs.push(SavedTab { url, title });
                }
            }
            rest = &chunk[end + 1..];
        }
        if tabs.is_empty() {
            return None;
        }
        let active = active.min(tabs.len() - 1);
        Some(Self { active, tabs })
    }
}

/// Directorio de datos del navegador.
pub fn data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("silversurfer");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/share/silversurfer");
    }
    PathBuf::from("/tmp/silversurfer")
}

pub fn session_path() -> PathBuf {
    data_dir().join("session.json")
}

pub fn load_from(path: &Path) -> Option<Snapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    Snapshot::decode(&text)
}

pub fn load() -> Option<Snapshot> {
    load_from(&session_path())
}

pub fn save_to(path: &Path, snap: &Snapshot) -> Result<(), String> {
    if snap.tabs.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("sesión: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, snap.encode()).map_err(|e| format!("sesión: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("sesión: {e}"))
}

pub fn save(snap: &Snapshot) -> Result<(), String> {
    save_to(&session_path(), snap)
}

pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::from('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn find_number(text: &str, key: &str) -> Option<usize> {
    let i = text.find(key)?;
    let after = text[i + key.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

pub(crate) fn find_string(text: &str, key: &str) -> Option<String> {
    let i = text.find(key)?;
    let after = text[i + key.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let after = after.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = after.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn round_trip_keeps_tabs_and_active() {
        let snap = Snapshot {
            active: 1,
            tabs: vec![
                SavedTab {
                    url: "https://example.com/".into(),
                    title: "Example".into(),
                },
                SavedTab {
                    url: "file:///tmp/a.html".into(),
                    title: "hola \"mundo\"".into(),
                },
            ],
        };
        let text = snap.encode();
        let back = Snapshot::decode(&text).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn empty_or_garbage_is_none() {
        assert!(Snapshot::decode("").is_none());
        assert!(Snapshot::decode("{}").is_none());
        assert!(Snapshot::decode("{\"active\":0,\"tabs\":[]}").is_none());
    }

    #[test]
    fn save_and_load_use_the_same_file() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("silversurfer-session-{nanos}"));
        let path = dir.join("session.json");
        let snap = Snapshot {
            active: 0,
            tabs: vec![SavedTab {
                url: "https://servo.org/".into(),
                title: "Servo".into(),
            }],
        };
        save_to(&path, &snap).unwrap();
        let back = load_from(&path).unwrap();
        assert_eq!(back, snap);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn active_is_clamped_to_the_last_tab() {
        let text = r#"{ "active": 99, "tabs": [ { "url": "https://a/", "title": "A" } ] }"#;
        let snap = Snapshot::decode(text).unwrap();
        assert_eq!(snap.active, 0);
        assert_eq!(snap.tabs.len(), 1);
    }
}
