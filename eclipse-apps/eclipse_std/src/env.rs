use ::alloc::string::String;
pub type OsStr = str;
pub type OsString = String;

pub fn set_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(_k: K, _v: V) {
    // TODO: Implement SYS_SETENV
}

pub fn remove_var<K: AsRef<OsStr>>(_k: K) {
    // TODO: Implement SYS_UNSETENV
}

pub fn var<K: AsRef<OsStr>>(_k: K) -> core::result::Result<String, VarError> {
    Err(VarError::NotPresent)
}

pub fn home_dir() -> Option<crate::path::PathBuf> {
    // For now, default to root directory
    Some(crate::path::PathBuf::from(String::from("/")))
}

#[derive(Debug, PartialEq, Eq)]
pub enum VarError {
    NotPresent,
    NotUnicode(OsString),
}
