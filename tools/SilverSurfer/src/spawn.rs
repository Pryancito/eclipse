//! Abrir otra ventana SilverSurfer (proceso nuevo).

use std::path::PathBuf;
use std::process::Command;

pub fn new_window(url: Option<&str>) -> Result<(), String> {
    spawn(url, false)
}

pub fn private_window(url: Option<&str>) -> Result<(), String> {
    spawn(url, true)
}

fn spawn(url: Option<&str>, private: bool) -> Result<(), String> {
    let program = current_binary()?;
    let mut cmd = Command::new(&program);
    if private {
        cmd.arg("--private");
    }
    match url {
        Some(u) if !u.is_empty() => {
            cmd.arg(u);
        }
        _ if !private => {
            cmd.arg("--fresh");
        }
        _ => {}
    }
    cmd.spawn()
        .map_err(|e| format!("no se pudo abrir ventana: {e}"))?;
    Ok(())
}

fn current_binary() -> Result<PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.is_file() {
            return Ok(exe);
        }
    }
    for name in ["silversurfer", "SilverSurfer"] {
        if let Some(p) = find_in_path(name) {
            return Ok(p);
        }
    }
    Err("no se encuentra el binario de SilverSurfer".into())
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for dir in ["/usr/local/bin", "/usr/bin", "/bin"] {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_binary_resolves() {
        assert!(current_binary().is_ok());
    }
}
