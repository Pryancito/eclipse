use ::alloc::string::String;
use ::alloc::vec::Vec;
use crate::sync::Mutex;

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
    env.retain(|(a, _)| a != &key);
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

pub fn home_dir() -> Option<crate::path::PathBuf> {
    Some(crate::path::PathBuf::from(String::from("/")))
}

#[derive(Debug, PartialEq, Eq)]
pub enum VarError {
    NotPresent,
    NotUnicode(OsString),
}
