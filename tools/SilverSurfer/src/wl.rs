//! Connect to labwc even when the process was not started from the session.
//!
//! eclipse-init does not export `WAYLAND_DISPLAY`. The pure-Rust wayland
//! client then refuses to connect, while libwayland would have tried
//! `wayland-0`. SilverSurfer uses libwayland (EGL needs its pointers), and
//! still probes the usual sockets so a launcher without the variable works.

use std::path::PathBuf;

use wayland_client::Connection;

const RUNTIME_DIRS: &[&str] = &["/run/user/0", "/run/user/1000", "/tmp"];

pub fn connect() -> Result<Connection, String> {
    if let Ok(conn) = Connection::connect_to_env() {
        return Ok(conn);
    }
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            dirs.push(path);
        }
    }
    for dir in RUNTIME_DIRS {
        let path = PathBuf::from(dir);
        if !dirs.contains(&path) {
            dirs.push(path);
        }
    }
    let hint = std::env::var("WAYLAND_DISPLAY").ok();
    for dir in &dirs {
        let entries: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        for name in names_in(&entries, hint.as_deref()) {
            let path = dir.join(&name);
            if let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) {
                if let Ok(conn) = Connection::from_socket(stream) {
                    std::env::set_var("XDG_RUNTIME_DIR", dir);
                    std::env::set_var("WAYLAND_DISPLAY", &name);
                    return Ok(conn);
                }
            }
        }
    }
    Err("no hay un compositor Wayland (¿labwc está arrancado?)".into())
}

fn names_in(entries: &[String], hint: Option<&str>) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(hint) = hint.filter(|h| !h.is_empty() && !h.contains('/')) {
        names.push(hint.to_string());
    }
    for i in 0..8 {
        let name = format!("wayland-{i}");
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for name in entries {
        if name.starts_with("wayland-") && !name.ends_with(".lock") && !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_comes_first_and_locks_are_ignored() {
        let entries = vec![
            "wayland-9".into(),
            "wayland-1.lock".into(),
            "other".into(),
            "wayland-2".into(),
        ];
        let names = names_in(&entries, Some("wayland-3"));
        assert_eq!(names[0], "wayland-3");
        assert!(names.contains(&"wayland-9".to_string()));
        assert!(names.contains(&"wayland-2".to_string()));
        assert!(!names.iter().any(|n| n.ends_with(".lock")));
        assert!(!names.iter().any(|n| n == "other"));
        assert_eq!(names.iter().filter(|n| *n == "wayland-3").count(), 1);
    }

    #[test]
    fn a_path_is_not_a_socket_name() {
        let names = names_in(&[], Some("/run/user/1000/wayland-0"));
        assert_eq!(names[0], "wayland-0");
        assert!(!names.iter().any(|n| n.contains('/')));
    }
}
