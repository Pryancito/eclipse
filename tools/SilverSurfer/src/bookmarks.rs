//! Marcadores simples (JSON en XDG data) y página `about:bookmarks`.

use std::path::{Path, PathBuf};

use crate::session::{data_dir, find_string, json_string, leaf_objects};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Bookmarks {
    pub items: Vec<Bookmark>,
}

impl Bookmarks {
    pub fn encode(&self) -> String {
        let mut out = String::from("{\n  \"bookmarks\": [\n");
        for (i, item) in self.items.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"url\": {},\n", json_string(&item.url)));
            out.push_str(&format!("      \"title\": {}\n", json_string(&item.title)));
            out.push_str("    }");
            if i + 1 < self.items.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    pub fn decode(text: &str) -> Self {
        let mut items = Vec::new();
        for obj in leaf_objects(text) {
            if obj.contains("\"url\"") {
                if let Some(url) = find_string(obj, "\"url\"") {
                    if !url.is_empty() {
                        let title = find_string(obj, "\"title\"").unwrap_or_default();
                        items.push(Bookmark { url, title });
                    }
                }
            }
        }
        Self { items }
    }

    pub fn contains(&self, url: &str) -> bool {
        self.items.iter().any(|b| b.url == url)
    }

    /// Añade o quita. `true` si quedó marcado.
    pub fn toggle(&mut self, url: &str, title: &str) -> bool {
        let url = url.trim();
        if url.is_empty() {
            return false;
        }
        if let Some(i) = self.items.iter().position(|b| b.url == url) {
            self.items.remove(i);
            false
        } else {
            self.items.insert(
                0,
                Bookmark {
                    url: url.to_string(),
                    title: title.trim().to_string(),
                },
            );
            true
        }
    }
}

pub fn bookmarks_path() -> PathBuf {
    data_dir().join("bookmarks.json")
}

pub fn load_from(path: &Path) -> Bookmarks {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Bookmarks::default();
    };
    Bookmarks::decode(&text)
}

pub fn load() -> Bookmarks {
    load_from(&bookmarks_path())
}

pub fn save_to(path: &Path, bookmarks: &Bookmarks) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("marcadores: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bookmarks.encode()).map_err(|e| format!("marcadores: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("marcadores: {e}"))
}

pub fn save(bookmarks: &Bookmarks) -> Result<(), String> {
    save_to(&bookmarks_path(), bookmarks)
}

/// `true` si el URL quedó marcado.
pub fn toggle(url: &str, title: &str) -> bool {
    let mut bookmarks = load();
    let marked = bookmarks.toggle(url, title);
    let _ = save(&bookmarks);
    marked
}

pub fn page_url() -> url::Url {
    let bookmarks = load();
    let mut body = String::from(
        "<!doctype html><meta charset=utf-8><title>Marcadores — SilverSurfer</title>\
<style>body{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}\
a{color:#7ec8ff;text-decoration:none}a:hover{text-decoration:underline}\
li{margin:.4rem 0}.t{color:#999;margin-left:.5rem;font-size:12px}\
h1{font-size:1.4rem}</style><h1>Marcadores</h1>",
    );
    if bookmarks.items.is_empty() {
        body.push_str("<p>No hay marcadores. Usa Ctrl+D en una página.</p>");
    } else {
        body.push_str("<ul>");
        for item in &bookmarks.items {
            let title = if item.title.is_empty() {
                item.url.as_str()
            } else {
                item.title.as_str()
            };
            body.push_str(&format!(
                "<li><a href=\"{}\">{}</a><span class=t>{}</span></li>",
                html_escape(&item.url),
                html_escape(title),
                html_escape(&item.url),
            ));
        }
        body.push_str("</ul>");
    }
    let path = std::env::temp_dir().join("silversurfer-bookmarks.html");
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
    fn toggle_adds_and_removes() {
        let mut b = Bookmarks::default();
        assert!(b.toggle("https://a/", "A"));
        assert!(b.contains("https://a/"));
        assert!(!b.toggle("https://a/", "A"));
        assert!(!b.contains("https://a/"));
    }

    #[test]
    fn round_trip_file() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("silversurfer-bookmarks-{nanos}"));
        let path = dir.join("bookmarks.json");
        let mut b = Bookmarks::default();
        b.toggle("https://servo.org/", "Servo");
        save_to(&path, &b).unwrap();
        let back = load_from(&path);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].url, "https://servo.org/");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn page_url_is_a_local_file() {
        let url = page_url();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-bookmarks.html"));
    }
}
