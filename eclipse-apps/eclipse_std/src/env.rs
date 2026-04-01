use ::alloc::string::String;
use ::alloc::vec::Vec;
use crate::sync::Mutex;
use crate::path::PathBuf;

// ============================================================================
// Argumentos del proceso (argv)
// ============================================================================

static ARGV: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Devuelve una copia de los argumentos del proceso (argv[0], argv[1], ...).
/// Se rellena una sola vez en `init_runtime()` leyendo del kernel.
pub fn args() -> Vec<String> {
    ARGV.lock().clone()
}

/// Inicializar ARGV leyendo del kernel (syscall sys_get_process_args = 543).
/// Formato recibido: "argv0\0argv1\0argv2\0..."
pub(crate) fn init_args() {
    let mut buf = [0u8; 4096];
    let n = eclipse_syscall::call::get_process_args(&mut buf);
    if n > 0 {
        let mut argv: Vec<String> = buf[..n]
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from(core::str::from_utf8(s).unwrap_or("")))
            .collect();
        // Asegurarse de que argv[0] sea siempre el nombre del proceso
        if argv.is_empty() { argv.push(String::from("?")); }
        *ARGV.lock() = argv;
    }
}

pub type OsStr = str;
pub type OsString = String;

static ENV: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub fn set_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(k: K, v: V) {
    let key = String::from(k.as_ref());
    let val = String::from(v.as_ref());
    let mut env = ENV.lock();
    if let Some((_, existing)) = env.iter_mut().find(|(a, _)| a == &key) {
        *existing = val;
    } else {
        env.push((key, val));
    }
}

pub fn remove_var<K: AsRef<OsStr>>(k: K) {
    let key = String::from(k.as_ref());
    let mut env = ENV.lock();
    env.retain(|(a, _)| a != &key)
}

pub fn var<K: AsRef<OsStr>>(k: K) -> core::result::Result<String, VarError> {
    let key = String::from(k.as_ref());
    let env = ENV.lock();
    for (a, b) in env.iter() {
        if a == &key {
            return Ok(b.clone());
        }
    }
    Err(VarError::NotPresent)
}

pub fn home_dir() -> Option<PathBuf> {
    Some(PathBuf::from(String::from("/")))
}

#[derive(Debug, PartialEq, Eq)]
pub enum VarError {
    NotPresent,
    NotUnicode(OsString),
}

pub mod consts {
    pub const ARCH: &str = "x86_64";
    pub const DLL_PREFIX: &str = "lib";
    pub const DLL_SUFFIX: &str = ".so";
    pub const EXE_PREFIX: &str = "";
    pub const EXE_SUFFIX: &str = "";
    pub const FAMILY: &str = "unix";
    pub const OS: &str = "eclipse";
}
