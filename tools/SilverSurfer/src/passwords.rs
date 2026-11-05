//! Almacén local de credenciales (F3-lite).
//!
//! Fichero XDG `passwords.json`. Cada secreto va cifrado con
//! ChaCha20-Poly1305 (`"v2:" + base64(nonce ‖ cifrado)`), con el origen y el
//! usuario como datos asociados para que no se puedan intercambiar entradas.
//! La clave son 32 bytes aleatorios en `passwords.key` (0600), aparte del
//! almacén: copiar o sincronizar `passwords.json` solo no revela nada. Quien
//! pueda leer los dos ficheros del usuario puede descifrarlos, como en
//! Firefox sin contraseña maestra.
//!
//! Los almacenes antiguos (XOR + base64, sin prefijo) se leen y se reescriben
//! cifrados al cargarlos. En ventana `--private` no se lee ni escribe.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::session::{data_dir, find_string, json_string, leaf_objects};

/// Clave del formato antiguo; solo para leer almacenes sin migrar.
const LEGACY_XOR_KEY: &[u8] = b"EclipseOS-SilverSurfer-passwords-v1";
const V2: &str = "v2:";
const NONCE_LEN: usize = 12;

/// Clave del almacén.
#[derive(Clone)]
pub struct Cipher(ChaCha20Poly1305);

impl Cipher {
    pub fn from_key(key: &[u8; 32]) -> Self {
        Self(ChaCha20Poly1305::new(Key::from_slice(key)))
    }

    fn seal(&self, entry: &Entry) -> Result<String, String> {
        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        let aad = associated_data(&entry.origin, &entry.username);
        let sealed = self
            .0
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: entry.secret.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| "contraseñas: no se pudo cifrar".to_string())?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&sealed);
        Ok(format!("{V2}{}", base64_encode(&blob)))
    }

    fn open(&self, origin: &str, username: &str, stored: &str) -> Option<String> {
        let blob = base64_decode(stored.strip_prefix(V2)?).ok()?;
        if blob.len() < NONCE_LEN {
            return None;
        }
        let (nonce, sealed) = blob.split_at(NONCE_LEN);
        let aad = associated_data(origin, username);
        let plain = self
            .0
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .ok()?;
        String::from_utf8(plain).ok()
    }
}

fn associated_data(origin: &str, username: &str) -> Vec<u8> {
    let mut aad = origin.as_bytes().to_vec();
    aad.push(0);
    aad.extend_from_slice(username.as_bytes());
    aad
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub origin: String,
    pub username: String,
    pub secret: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Store {
    pub entries: Vec<Entry>,
    /// Alguna entrada venía en el formato antiguo y hay que reescribirla.
    pub legacy: bool,
}

impl Store {
    pub fn encode(&self, cipher: &Cipher) -> Result<String, String> {
        let mut out = String::from("{\n  \"passwords\": [\n");
        for (i, e) in self.entries.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"origin\": {},\n", json_string(&e.origin)));
            out.push_str(&format!(
                "      \"username\": {},\n",
                json_string(&e.username)
            ));
            out.push_str(&format!(
                "      \"secret\": {}\n",
                json_string(&cipher.seal(e)?)
            ));
            out.push_str("    }");
            if i + 1 < self.entries.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        Ok(out)
    }

    pub fn decode(text: &str, cipher: &Cipher) -> Self {
        let mut entries = Vec::new();
        let mut legacy = false;
        for obj in leaf_objects(text) {
            if obj.contains("\"origin\"") && obj.contains("\"secret\"") {
                if let (Some(origin), Some(username), Some(secret)) = (
                    find_string(obj, "\"origin\""),
                    find_string(obj, "\"username\""),
                    find_string(obj, "\"secret\""),
                ) {
                    if origin.is_empty() {
                        continue;
                    }
                    let secret = if secret.starts_with(V2) {
                        match cipher.open(&origin, &username, &secret) {
                            Some(plain) => plain,
                            None => {
                                // Otra clave o fichero alterado: se descarta
                                // en vez de rellenar formularios con basura.
                                eprintln!(
                                    "silversurfer: contraseña de {origin} ilegible \
(¿falta passwords.key?)"
                                );
                                continue;
                            }
                        }
                    } else {
                        legacy = true;
                        deobfuscate(&secret)
                    };
                    entries.push(Entry {
                        origin,
                        username,
                        secret,
                    });
                }
            }
        }
        Self { entries, legacy }
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

pub fn key_path() -> PathBuf {
    data_dir().join("passwords.key")
}

/// Lee la clave de `path`; si no existe, la crea (0600, sin pisar otra).
pub fn load_or_create_key(path: &Path) -> Result<Cipher, String> {
    let mut key = [0u8; 32];
    match std::fs::File::open(path) {
        Ok(mut file) => {
            file.read_exact(&mut key)
                .map_err(|e| format!("passwords.key: {e}"))?;
            return Ok(Cipher::from_key(&key));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("passwords.key: {e}")),
    }
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    random_bytes(&mut key)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(&key)
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("passwords.key: {e}"))?;
            Ok(Cipher::from_key(&key))
        }
        // Otra ventana la creó a la vez: vale la suya.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create_key(path),
        Err(e) => Err(format!("passwords.key: {e}")),
    }
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|e| format!("passwords: {e}"))
}

/// Carga el almacén. Si venía en el formato antiguo, lo reescribe cifrado.
pub fn load_from(path: &Path, key: &Path) -> Store {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Store::default();
    };
    let cipher = match load_or_create_key(key) {
        Ok(cipher) => cipher,
        Err(err) => {
            eprintln!("silversurfer: {err}");
            return Store::default();
        }
    };
    let mut store = Store::decode(&text, &cipher);
    if store.legacy {
        match write_store(path, &store, &cipher) {
            Ok(()) => store.legacy = false,
            Err(err) => eprintln!("silversurfer: {err}"),
        }
    }
    store
}

pub fn load() -> Store {
    load_from(&passwords_path(), &key_path())
}

pub fn save_to(path: &Path, key: &Path, store: &Store) -> Result<(), String> {
    let cipher = load_or_create_key(key)?;
    write_store(path, store, &cipher)
}

fn write_store(path: &Path, store: &Store, cipher: &Cipher) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let text = store.encode(cipher)?;
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| format!("passwords: {e}"))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("passwords: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("passwords: {e}"))
}

pub fn save(store: &Store) -> Result<(), String> {
    save_to(&passwords_path(), &key_path(), store)
}

/// Bytes del generador del núcleo (getrandom(2), o `/dev/urandom`).
fn random_bytes(buf: &mut [u8]) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let mut filled = 0;
        while filled < buf.len() {
            let rest = &mut buf[filled..];
            // SAFETY: `rest` es un búfer válido de `rest.len()` bytes.
            let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
            if n > 0 {
                filled += n as usize;
            } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
        if filled == buf.len() {
            return Ok(());
        }
    }
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| format!("aleatoriedad: {e}"))
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
    let l = crate::lang();
    let mut body = format!(
        "<!doctype html><meta charset=utf-8><title>{tab} — SilverSurfer</title>\
<style>body{{font:14px/1.45 system-ui,sans-serif;margin:2rem;background:#111;color:#eee}}\
.t{{color:#999;font-size:12px}}li{{margin:.4rem 0}}h1{{font-size:1.35rem}}\
code{{background:#222;padding:.1rem .3rem}}</style>\
<h1>{head}</h1>\
<p>{how}</p>",
        tab = l.s(crate::Key::www_passwords),
        head = l.s(crate::Key::www_passwords_saved),
        how = l.s(crate::Key::www_passwords_how),
    );
    if store.entries.is_empty() {
        body.push_str(&format!("<p>{}</p>", l.s(crate::Key::www_empty)));
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

fn deobfuscate(encoded: &str) -> String {
    let Ok(bytes) = base64_decode(encoded) else {
        return String::new();
    };
    let plain: Vec<u8> = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ LEGACY_XOR_KEY[i % LEGACY_XOR_KEY.len()])
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
        let c = if chunk[2] == b'=' { 0 } else { val(chunk[2])? };
        let d = if chunk[3] == b'=' { 0 } else { val(chunk[3])? };
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

/// Literal de cadena JavaScript (entre comillas dobles) para `s`.
pub fn js_str(s: &str) -> String {
    let mut out = String::from('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
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

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ss-pass-{tag}-{nanos}"))
    }

    fn cipher() -> Cipher {
        Cipher::from_key(&[7u8; 32])
    }

    #[test]
    fn encrypted_round_trip_hides_the_secret() {
        let mut store = Store::default();
        store.upsert("https://a.test", "ada", "páss word! 🔐");
        let text = store.encode(&cipher()).unwrap();
        assert!(!text.contains("páss"));
        assert!(text.contains("\"v2:"));
        let back = Store::decode(&text, &cipher());
        assert_eq!(back.entries, store.entries);
        assert!(!back.legacy);
        // Nonce nuevo en cada guardado.
        assert_ne!(text, store.encode(&cipher()).unwrap());
    }

    #[test]
    fn wrong_key_or_swapped_entry_does_not_decrypt() {
        let mut store = Store::default();
        store.upsert("https://a.test", "ada", "s3cret");
        let text = store.encode(&cipher()).unwrap();
        let other = Cipher::from_key(&[8u8; 32]);
        assert!(Store::decode(&text, &other).entries.is_empty());
        // El mismo secreto pegado en otro origen no se abre.
        let moved = text.replace("https://a.test", "https://evil.test");
        assert!(Store::decode(&moved, &cipher()).entries.is_empty());
    }

    #[test]
    fn legacy_store_is_read_and_migrated() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("passwords.json");
        let key = dir.join("passwords.key");
        let xored: Vec<u8> = "viejo"
            .bytes()
            .enumerate()
            .map(|(i, b)| b ^ LEGACY_XOR_KEY[i % LEGACY_XOR_KEY.len()])
            .collect();
        let legacy = format!(
            "{{\"passwords\": [{{\"origin\": \"https://a.test\", \"username\": \"u\", \
\"secret\": \"{}\"}}]}}",
            base64_encode(&xored)
        );
        std::fs::write(&path, legacy).unwrap();
        let store = load_from(&path, &key);
        assert_eq!(store.entries[0].secret, "viejo");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("\"v2:"));
        assert_eq!(load_from(&path, &key).entries[0].secret, "viejo");
        let _ = std::fs::remove_dir_all(dir);
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
        assert!(store
            .find_for_url("http://example.com:8080/login")
            .is_none());
        assert_eq!(
            origin_of("https://example.com:8443/a").as_deref(),
            Some("https://example.com:8443")
        );
    }

    #[test]
    fn file_round_trip() {
        let dir = temp_dir("file");
        let path = dir.join("passwords.json");
        let key = dir.join("passwords.key");
        let mut store = Store::default();
        store.upsert("https://a.test", "u", "s3cret");
        save_to(&path, &key, &store).unwrap();
        let back = load_from(&path, &key);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].secret, "s3cret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&key), 0o600);
            assert_eq!(std::fs::metadata(&key).unwrap().len(), 32);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fill_script_embeds_values() {
        let s = fill_script("ada", "x\"y");
        assert!(s.contains("ada"));
        assert!(s.contains("\\\""));
    }
}
