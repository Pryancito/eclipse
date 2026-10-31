//! Historial de navegación (JSON en XDG data).
//!
//! Formato deliberadamente trivial, sin `serde`: una lista de visitas
//! `{url,title}` (más reciente primero). Tope de 200 entradas.

use std::path::{Path, PathBuf};

use crate::session::{data_dir, find_string, json_string, leaf_objects};

const MAX_VISITS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visit {
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct History {
    pub visits: Vec<Visit>,
}

impl History {
    pub fn encode(&self) -> String {
        let mut out = String::from("{\n  \"visits\": [\n");
        for (i, visit) in self.visits.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"url\": {},\n", json_string(&visit.url)));
            out.push_str(&format!("      \"title\": {}\n", json_string(&visit.title)));
            out.push_str("    }");
            if i + 1 < self.visits.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    pub fn decode(text: &str) -> Self {
        let mut visits = Vec::new();
        for obj in leaf_objects(text) {
            if obj.contains("\"url\"") {
                if let Some(url) = find_string(obj, "\"url\"") {
                    if !url.is_empty() && !url.starts_with("about:") {
                        let title = find_string(obj, "\"title\"").unwrap_or_default();
                        visits.push(Visit { url, title });
                    }
                }
            }
        }
        Self { visits }
    }

    /// Inserta o mueve al frente. Ignora `about:` y URLs vacías.
    pub fn record(&mut self, url: &str, title: &str) {
        let url = url.trim();
        if url.is_empty() || url.starts_with("about:") {
            return;
        }
        self.visits.retain(|v| v.url != url);
        self.visits.insert(
            0,
            Visit {
                url: url.to_string(),
                title: title.trim().to_string(),
            },
        );
        if self.visits.len() > MAX_VISITS {
            self.visits.truncate(MAX_VISITS);
        }
    }

    /// Coincidencias por prefijo de URL o de título (case-insensitive).
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<&Visit> {
        let needle = prefix.trim().to_ascii_lowercase();
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        self.visits
            .iter()
            .filter(|v| {
                v.url.to_ascii_lowercase().contains(&needle)
                    || v.title.to_ascii_lowercase().contains(&needle)
            })
            .take(limit)
            .collect()
    }

    /// Primera sugerencia que completa el texto escrito (prefijo de URL).
    pub fn autocomplete(&self, typed: &str) -> Option<&Visit> {
        let typed = typed.trim();
        if typed.is_empty() {
            return None;
        }
        let lower = typed.to_ascii_lowercase();
        self.visits.iter().find(|v| {
            let url = v.url.to_ascii_lowercase();
            url.starts_with(&lower) && url.len() > lower.len()
        })
    }
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

pub fn load_from(path: &Path) -> History {
    let Ok(text) = std::fs::read_to_string(path) else {
        return History::default();
    };
    History::decode(&text)
}

pub fn load() -> History {
    load_from(&history_path())
}

pub fn save_to(path: &Path, history: &History) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("historial: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, history.encode()).map_err(|e| format!("historial: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("historial: {e}"))
}

pub fn save(history: &History) -> Result<(), String> {
    save_to(&history_path(), history)
}

pub fn record(url: &str, title: &str) {
    let mut history = load();
    history.record(url, title);
    let _ = save(&history);
}

/// Página `about:history` como fichero temporal.
pub fn page_url() -> url::Url {
    let history = load();
    let mut body = String::from(
        "<!doctype html><meta charset=utf-8><title>Historial — SilverSurfer</title>\
<style>body{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}\
a{color:#7ec8ff;text-decoration:none}a:hover{text-decoration:underline}\
li{margin:.4rem 0}.t{color:#999;margin-left:.5rem;font-size:12px}\
h1{font-size:1.4rem}</style><h1>Historial</h1>",
    );
    if history.visits.is_empty() {
        body.push_str("<p>Aún no hay visitas.</p>");
    } else {
        body.push_str("<ol>");
        for visit in &history.visits {
            let title = if visit.title.is_empty() {
                visit.url.as_str()
            } else {
                visit.title.as_str()
            };
            body.push_str(&format!(
                "<li><a href=\"{}\">{}</a><span class=t>{}</span></li>",
                html_escape(&visit.url),
                html_escape(title),
                html_escape(&visit.url),
            ));
        }
        body.push_str("</ol>");
    }
    let path = std::env::temp_dir().join("silversurfer-history.html");
    let _ = std::fs::write(&path, body);
    url::Url::from_file_path(&path).expect("el directorio temporal es absoluto")
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn record_moves_to_front_and_dedups() {
        let mut h = History::default();
        h.record("https://a/", "A");
        h.record("https://b/", "B");
        h.record("https://a/", "A2");
        assert_eq!(h.visits.len(), 2);
        assert_eq!(h.visits[0].url, "https://a/");
        assert_eq!(h.visits[0].title, "A2");
        assert_eq!(h.visits[1].url, "https://b/");
    }

    #[test]
    fn about_urls_are_ignored() {
        let mut h = History::default();
        h.record("about:blank", "x");
        h.record("about:history", "y");
        assert!(h.visits.is_empty());
    }

    #[test]
    fn autocomplete_and_suggest() {
        let mut h = History::default();
        h.record("https://servo.org/demo", "Demo");
        h.record("https://example.com/", "Example");
        let ac = h.autocomplete("https://serv").unwrap();
        assert_eq!(ac.url, "https://servo.org/demo");
        let hits = h.suggest("exam", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Example");
    }

    #[test]
    fn round_trip_file() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("silversurfer-history-{nanos}"));
        let path = dir.join("history.json");
        let mut h = History::default();
        h.record("https://a/", "A \"quoted\"");
        save_to(&path, &h).unwrap();
        let back = load_from(&path);
        assert_eq!(back.visits.len(), 1);
        assert_eq!(back.visits[0].title, "A \"quoted\"");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn page_url_is_a_local_file() {
        let url = page_url();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-history.html"));
    }
}
