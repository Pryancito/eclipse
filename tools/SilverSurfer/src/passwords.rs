//! Almacén local de credenciales (F3-lite).
//!
//! Fichero XDG `passwords.json`. El secreto se guarda ofuscado (XOR + base64),
//! no es cifrado fuerte: protege solo frente a lectura casual del disco.
//! En ventana `--private` no se lee ni escribe.

use std::path::{Path, PathBuf};

use crate::session::{data_dir, find_string, json_string, leaf_objects};

const XOR_KEY: &[u8] = b"EclipseOS-SilverSurfer-passwords-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub origin: String,
    pub username: String,
    pub secret: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Store {
    pub entries: Vec<Entry>,
}

impl Store {
    pub fn encode(&self) -> String {
        let mut out = String::from("{\n  \"passwords\": [\n");
        for (i, e) in self.entries.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"origin\": {},\n", json_string(&e.origin)));
            out.push_str(&format!("      \"username\": {},\n", json_string(&e.username)));
            out.push_str(&format!(
                "      \"secret\": {}\n",
                json_string(&obfuscate(&e.secret))
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
            if obj.contains("\"origin\"") && obj.contains("\"secret\"") {
                if let (Some(origin), Some(username), Some(secret)) = (
                    find_string(obj, "\"origin\""),
                    find_string(obj, "\"username\""),
                    find_string(obj, "\"secret\""),
                ) {
                    if !origin.is_empty() {
                        entries.push(Entry {
                            origin,
                            username,
                            secret: deobfuscate(&secret),
                        });
                    }
                }
            }
        }
        Self { entries }
    }

    pub fn find_for_url(&self, url: &str) -> Option<&Entry> {
        let origin = origin_of(url)?;
        self.entries.iter().find(|e| e.origin == origin)
    }

    pub fn upsert(&mut self, origin: &str, username: &str, secret: &str) {
        let origin = origin.trim();
        if origin.is_empty() || secret.is_empty() {
            return;
        }
        if let Some(e) = self.entries.iter_mut().find(|e| e.origin == origin) {
            e.username = username.to_string();
            e.secret = secret.to_string();
        } else {
            self.entries.push(Entry {
                origin: origin.to_string(),
                username: username.to_string(),
                secret: secret.to_string(),
            });
        }
    }

    pub fn remove(&mut self, origin: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.origin != origin);
        self.entries.len() != before
    }
}

pub fn passwords_path() -> PathBuf {
    data_dir().join("passwords.json")
}

pub fn load_from(path: &Path) -> Store {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Store::default();
    };
    Store::decode(&text)
}

pub fn load() -> Store {
    load_from(&passwords_path())
}

pub fn save_to(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("passwords: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, store.encode()).map_err(|e| format!("passwords: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("passwords: {e}"))
}

pub fn save(store: &Store) -> Result<(), String> {
    save_to(&passwords_path(), store)
}

pub fn origin_of(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

pub fn page_url() -> url::Url {
    let store = load();
    let mut body = String::from(
        "<!doctype html><meta charset=utf-8><title>Contraseñas — SilverSurfer</title>\
<style>body{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}\
.t{color:#999;font-size:12px}li{margin:.4rem 0}h1{font-size:1.35rem}\
code{background:#222;padding:.1rem .3rem}</style>\
<h1>Contraseñas guardadas</h1>\
<p>Ctrl+Shift+L guarda el login visible. Autocompletado al cargar el origen.\
 Almacén ofuscado en <code>passwords.json</code> (no es cifrado fuerte).</p>",
    );
    if store.entries.is_empty() {
        body.push_str("<p>Vacío.</p>");
    } else {
        body.push_str("<ul>");
        for e in &store.entries {
            body.push_str(&format!(
                "<li><strong>{}</strong> — {} <span class=t>(••••)</span></li>",
                html_escape(&e.origin),
                html_escape(&e.username)
            ));
        }
        body.push_str("</ul>");
    }
    let path = std::env::temp_dir().join("silversurfer-passwords.html");
    let _ = std::fs::write(&path, body);
    url::Url::from_file_path(&path).expect("temp absoluto")
}

pub fn fill_script(username: &str, secret: &str) -> String {
    format!(
        "(function(){{try{{\
var u={user},p={pass};\
var inputs=document.querySelectorAll('input');\
var userEl=null,passEl=null;\
for(var i=0;i<inputs.length;i++){{\
var t=(inputs[i].type||'').toLowerCase();\
var n=((inputs[i].name||'')+' '+(inputs[i].autocomplete||'')+' '+(inputs[i].id||'')).toLowerCase();\
if(!passEl&&t==='password')passEl=inputs[i];\
if(!userEl&&(t==='email'||t==='text'||t==='tel')&&(n.indexOf('user')>=0||n.indexOf('email')>=0||n.indexOf('login')>=0||n.indexOf('account')>=0))userEl=inputs[i];\
}}\
if(!userEl){{for(var j=0;j<inputs.length;j++){{var tj=(inputs[j].type||'').toLowerCase();if(tj==='email'||tj==='text'){{userEl=inputs[j];break;}}}}}}\
if(userEl){{userEl.value=u;userEl.dispatchEvent(new Event('input',{{bubbles:true}}));}}\
if(passEl){{passEl.value=p;passEl.dispatchEvent(new Event('input',{{bubbles:true}}));}}\
return !!(userEl||passEl);\
}}catch(e){{return false;}}}})()",
        user = js_str(username),
        pass = js_str(secret),
    )
}

pub fn extract_script() -> &'static str {
    "(function(){try{\
var inputs=document.querySelectorAll('input');\
var user='',pass='',userEl=null,passEl=null;\
for(var i=0;i<inputs.length;i++){\
var t=(inputs[i].type||'').toLowerCase();\
if(!passEl&&t==='password'&&inputs[i].value)passEl=inputs[i];\
}\
if(!passEl)return '';\
for(var j=0;j<inputs.length;j++){\
var tj=(inputs[j].type||'').toLowerCase();\
if(tj==='password')continue;\
if(tj==='email'||tj==='text'||tj==='tel'){userEl=inputs[j];break;}\
}\
user=userEl?userEl.value:'';\
pass=passEl.value;\
return user+'\\n'+pass;\
}catch(e){return '';}})()"
}

fn obfuscate(plain: &str) -> String {
    let xored: Vec<u8> = plain
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ XOR_KEY[i % XOR_KEY.len()])
        .collect();
    base64_encode(&xored)
}

fn deobfuscate(encoded: &str) -> String {
    let Ok(bytes) = base64_decode(encoded) else {
        return String::new();
    };
    let plain: Vec<u8> = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ XOR_KEY[i % XOR_KEY.len()])
        .collect();
    String::from_utf8_lossy(&plain).into_owned()
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (a << 16) | (b << 8) | c;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>, ()> {
    fn val(c: u8) -> Result<u32, ()> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(()),
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return Err(());
    }
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            val(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            val(chunk[3])?
        };
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((n >> 16) & 255) as u8);
        if chunk[2] != b'=' {
            out.push(((n >> 8) & 255) as u8);
        }
        if chunk[3] != b'=' {
            out.push((n & 255) as u8);
        }
    }
    Ok(out)
}

fn js_str(s: &str) -> String {
    let mut out = String::from('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn obfuscate_round_trip() {
        let s = "páss word! 🔐";
        assert_eq!(deobfuscate(&obfuscate(s)), s);
    }

    #[test]
    fn upsert_and_find() {
        let mut store = Store::default();
        store.upsert("https://example.com", "ada", "lovelace");
        let e = store.find_for_url("https://example.com/login").unwrap();
        assert_eq!(e.username, "ada");
        assert_eq!(e.secret, "lovelace");
    }

    #[test]
    fn another_port_is_another_origin() {
        let mut store = Store::default();
        store.upsert("http://example.com", "ada", "lovelace");
        assert!(store.find_for_url("http://example.com:80/login").is_some());
        assert!(store.find_for_url("http://example.com:8080/login").is_none());
        assert_eq!(
            origin_of("https://example.com:8443/a").as_deref(),
            Some("https://example.com:8443")
        );
    }

    #[test]
    fn file_round_trip() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ss-pass-{nanos}"));
        let path = dir.join("passwords.json");
        let mut store = Store::default();
        store.upsert("https://a.test", "u", "s3cret");
        save_to(&path, &store).unwrap();
        let back = load_from(&path);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].secret, "s3cret");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fill_script_embeds_values() {
        let s = fill_script("ada", "x\"y");
        assert!(s.contains("ada"));
        assert!(s.contains("\\\""));
    }
}
