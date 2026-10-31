//! Descargas vía `lunarfile --save` + `curl`, y página `about:downloads`.
//!
//! Servo 0.7 no expone un hook de descarga al embedder: interceptamos
//! navegaciones a extensiones típicas de fichero y Ctrl+S sobre la URL
//! actual.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::files::{self, PickRequest};
use crate::session::{data_dir, find_string, json_string, leaf_objects};

const MAX_ENTRIES: usize = 100;

/// Extensiones que se tratan como descarga en vez de navegación.
const DOWNLOAD_EXTS: &[&str] = &[
    "7z", "apk", "bz2", "csv", "deb", "dmg", "doc", "docx", "epub", "exe",
    "gz", "iso", "jar", "odt", "ods", "odp", "pdf", "ppt", "pptx", "rar",
    "rpm", "rtf", "tar", "tgz", "torrent", "xz", "zip", "zst",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub url: String,
    pub path: String,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Log {
    pub entries: Vec<Entry>,
}

impl Log {
    pub fn encode(&self) -> String {
        let mut out = String::from("{\n  \"downloads\": [\n");
        for (i, entry) in self.entries.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"url\": {},\n", json_string(&entry.url)));
            out.push_str(&format!("      \"path\": {},\n", json_string(&entry.path)));
            out.push_str(&format!(
                "      \"ok\": {}\n",
                if entry.ok { "true" } else { "false" }
            ));
            out.push_str("    }");
            if i + 1 < self.entries.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    pub fn decode(text: &str) -> Self {
        let mut entries = Vec::new();
        for obj in leaf_objects(text) {
            if obj.contains("\"url\"") && obj.contains("\"path\"") {
                if let (Some(url), Some(path)) =
                    (find_string(obj, "\"url\""), find_string(obj, "\"path\""))
                {
                    let ok = obj.contains("\"ok\": true") || obj.contains("\"ok\":true");
                    entries.push(Entry { url, path, ok });
                }
            }
        }
        Self { entries }
    }

    pub fn push(&mut self, entry: Entry) {
        self.entries.insert(0, entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.truncate(MAX_ENTRIES);
        }
    }
}

pub fn downloads_path() -> PathBuf {
    data_dir().join("downloads.json")
}

pub fn load_from(path: &Path) -> Log {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Log::default();
    };
    Log::decode(&text)
}

pub fn load() -> Log {
    load_from(&downloads_path())
}

pub fn save_to(path: &Path, log: &Log) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("descargas: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, log.encode()).map_err(|e| format!("descargas: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("descargas: {e}"))
}

pub fn save(log: &Log) -> Result<(), String> {
    save_to(&downloads_path(), log)
}

/// Nombre sugerido a partir de la URL (último segmento del path).
pub fn suggested_name(url: &url::Url) -> String {
    let seg = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download");
    let name = seg.split('?').next().unwrap_or(seg);
    if name.is_empty() {
        "download".into()
    } else {
        name.to_string()
    }
}

/// `true` si la URL parece un fichero descargable (no HTML).
pub fn looks_like_download(url: &url::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https" | "ftp") {
        return false;
    }
    let name = suggested_name(url).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    DOWNLOAD_EXTS.iter().any(|e| *e == ext)
}

/// Abre lunarfile en modo guardar y descarga con `curl -L` en otro hilo.
///
/// Devuelve `Ok(Some(path))` si la descarga arrancó, `Ok(None)` si el
/// usuario canceló. `private` no deja rastro en `downloads.json`.
pub fn save_url(url: &url::Url, private: bool) -> Result<Option<PathBuf>, String> {
    let name = suggested_name(url);
    let paths = files::pick(&PickRequest {
        multiple: false,
        save_name: Some(name),
    })?;
    let Some(dest) = paths.into_iter().next() else {
        return Ok(None);
    };
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let url = url.clone();
    let target = dest.clone();
    std::thread::Builder::new()
        .name("silversurfer-download".into())
        .spawn(move || fetch(&url, &target, private))
        .map_err(|e| format!("descarga: {e}"))?;
    Ok(Some(dest))
}

fn fetch(url: &url::Url, dest: &Path, private: bool) {
    let ok = match Command::new("curl")
        .args(curl_args(url, dest))
        .status()
    {
        Ok(status) => status.success(),
        Err(err) => {
            eprintln!("silversurfer: curl: {err}");
            false
        }
    };
    if !private {
        let mut log = load();
        log.push(Entry {
            url: url.to_string(),
            path: dest.display().to_string(),
            ok,
        });
        let _ = save(&log);
    }
    if ok {
        eprintln!("silversurfer: descargado → {}", dest.display());
    } else {
        let _ = std::fs::remove_file(dest);
        eprintln!("silversurfer: curl falló al descargar {url}");
    }
}

fn curl_args(url: &url::Url, dest: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "-fsSL".into(),
        "--proto".into(),
        "=http,https,ftp".into(),
        "--proto-redir".into(),
        "=http,https".into(),
        "--output".into(),
        dest.as_os_str().to_owned(),
        "--".into(),
        url.as_str().into(),
    ]
}

pub fn page_url() -> url::Url {
    let log = load();
    let mut body = String::from(
        "<!doctype html><meta charset=utf-8><title>Descargas — SilverSurfer</title>\
<style>body{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}\
a{color:#7ec8ff;text-decoration:none}.ok{color:#8f8}.fail{color:#f88}\
li{margin:.45rem 0}.t{color:#999;font-size:12px;display:block}\
h1{font-size:1.4rem}</style><h1>Descargas</h1>",
    );
    if log.entries.is_empty() {
        body.push_str(
            "<p>Aún no hay descargas. Navega a un PDF/ZIP o usa Ctrl+S sobre una URL.</p>",
        );
    } else {
        body.push_str("<ol>");
        for entry in &log.entries {
            let klass = if entry.ok { "ok" } else { "fail" };
            body.push_str(&format!(
                "<li class={klass}><a href=\"file://{}\">{}</a>\
<span class=t>{}</span></li>",
                html_escape(&entry.path),
                html_escape(&entry.path),
                html_escape(&entry.url),
            ));
        }
        body.push_str("</ol>");
    }
    let path = std::env::temp_dir().join("silversurfer-downloads.html");
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

    #[test]
    fn pdf_and_zip_look_like_downloads() {
        let pdf = url::Url::parse("https://example.com/docs/guia.pdf").unwrap();
        assert!(looks_like_download(&pdf));
        assert_eq!(suggested_name(&pdf), "guia.pdf");
        let html = url::Url::parse("https://example.com/index.html").unwrap();
        assert!(!looks_like_download(&html));
        let page = url::Url::parse("https://example.com/ruta/").unwrap();
        assert!(!looks_like_download(&page));
    }

    #[test]
    fn curl_gets_the_url_after_the_option_terminator() {
        let url = url::Url::parse("https://example.com/a.zip").unwrap();
        let args = curl_args(&url, Path::new("/home/u/a.zip"));
        let n = args.len();
        assert_eq!(args[n - 2], "--");
        assert_eq!(args[n - 1], "https://example.com/a.zip");
        assert!(args.iter().any(|a| a == "=http,https,ftp"));
    }

    #[test]
    fn log_round_trip() {
        let mut log = Log::default();
        log.push(Entry {
            url: "https://a/x.zip".into(),
            path: "/tmp/x.zip".into(),
            ok: true,
        });
        let back = Log::decode(&log.encode());
        assert_eq!(back.entries.len(), 1);
        assert!(back.entries[0].ok);
        assert_eq!(back.entries[0].path, "/tmp/x.zip");
    }

    #[test]
    fn page_url_is_local() {
        let url = page_url();
        assert_eq!(url.scheme(), "file");
        assert!(url.path().ends_with("silversurfer-downloads.html"));
    }
}
