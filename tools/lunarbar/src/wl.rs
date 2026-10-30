//! Reaching the compositor when nothing in the environment points at it.
//!
//! `Connection::connect_to_env()` — like every wayland-client program — needs
//! `WAYLAND_DISPLAY` set and resolves it under `XDG_RUNTIME_DIR`. Three
//! situations on Eclipse OS leave those unset even though labwc is running:
//!   * launched from a bare init/VT shell that never sourced `/etc/profile`,
//!     so no `XDG_*` is exported;
//!   * labwc's autostart exports `XDG_RUNTIME_DIR` but not `WAYLAND_DISPLAY`;
//!   * **started by a service, not by the session**: `eclipse-init` gives
//!     every service `XDG_RUNTIME_DIR=/run/user/0` and deliberately no
//!     `WAYLAND_DISPLAY`, because the compositor is what creates the socket.
//!     That is the case of the XDG portal (`portal.service`), whose file
//!     chooser is `lunarfile --pick`: the dialog a browser asks for is started
//!     from a daemon that has no session in its environment.
//!
//! libwayland clients (foot) fall back to `wayland-0` by themselves and
//! connect; the pure-Rust wayland-client refuses without the variable and
//! `connect_to_env` fails. That difference alone is why lunarbg/lunarbar died
//! in autostart while foot ran fine, and why a portal file dialog exited
//! before it ever mapped — the browser then sees an answer with no paths in
//! it, which is a cancellation, so "nothing happens".
//!
//! So this module is the fallback every one of these binaries needs, in ONE
//! copy: try the standard connect, then look for a live `wayland-N` socket in
//! the usual runtime directories. [`names_in`] is the choice of candidates on
//! its own, so the order can be tested without a compositor.

use wayland_client::Connection;

/// The runtime directories to probe, most specific first. Eclipse OS runs as
/// root, so `/run/user/0` is the default `XDG_RUNTIME_DIR`.
const RUNTIME_DIRS: &[&str] = &["/run/user/0", "/run/user/1000", "/tmp"];

/// How many `wayland-N` names are tried in a directory whose listing says
/// nothing (an unreadable `/run/user/0`, which is what a client in a different
/// mount namespace sees).
const BLIND_SOCKETS: usize = 8;

/// The socket names to try in one directory, in order: the hint from
/// `WAYLAND_DISPLAY` first, then `wayland-0..7`, then anything else the
/// listing actually holds.
///
/// `entries` is the directory listing (empty when it could not be read) and
/// `hint` the value of `WAYLAND_DISPLAY`. A hint with a `/` in it is a path,
/// not a name, and is left to `connect_to_env`; the `.lock` files libwayland
/// keeps beside each socket are not sockets and are dropped, or the first
/// connect attempt of every run would be to a file that can never answer.
pub fn names_in(entries: &[String], hint: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(h) = hint.filter(|h| !h.is_empty() && !h.contains('/')) {
        names.push(h.to_string());
    }
    for i in 0..BLIND_SOCKETS {
        let n = format!("wayland-{i}");
        if !names.contains(&n) {
            names.push(n);
        }
    }
    for n in entries {
        if n.starts_with("wayland-") && !n.ends_with(".lock") && !names.contains(n) {
            names.push(n.clone());
        }
    }
    names
}

/// Connect to the Wayland compositor, auto-detecting the socket when the
/// environment does not point at one.
///
/// On success through the fallback, `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY`
/// are published into this process's own environment, so anything it spawns
/// lands in the same session.
pub fn connect() -> Result<Connection, String> {
    use std::path::PathBuf;

    // 1) Standard path: honours WAYLAND_SOCKET / WAYLAND_DISPLAY / XDG_RUNTIME_DIR.
    if let Ok(c) = Connection::connect_to_env() {
        return Ok(c);
    }

    // 2) Auto-detect. Probe runtime dirs, most specific first.
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(d);
        if p.is_absolute() {
            dirs.push(p);
        }
    }
    for d in RUNTIME_DIRS {
        let p = PathBuf::from(d);
        if !dirs.contains(&p) {
            dirs.push(p);
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
                    // Publish a working session for any child we spawn.
                    std::env::set_var("XDG_RUNTIME_DIR", dir);
                    std::env::set_var("WAYLAND_DISPLAY", &name);
                    return Ok(conn);
                }
            }
        }
    }
    Err("Could not find a running Wayland compositor (is labwc started?)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint goes first: a session on `wayland-1` must not have every client
    /// try `wayland-0` first, which on a machine with two compositors is the
    /// WRONG session rather than no session.
    #[test]
    fn the_hint_is_tried_before_the_blind_guesses() {
        let names = names_in(&[], Some("wayland-3"));
        assert_eq!(names.first().map(String::as_str), Some("wayland-3"));
        // And it is not repeated when the blind list reaches it.
        assert_eq!(names.iter().filter(|n| *n == "wayland-3").count(), 1);
        assert!(names.contains(&"wayland-0".to_string()), "{names:?}");
    }

    /// A path in `WAYLAND_DISPLAY` is `connect_to_env`'s business (it resolves
    /// absolute names itself); joining it onto a runtime directory would make
    /// a path that is simply not there.
    #[test]
    fn a_path_shaped_hint_is_not_used_as_a_name() {
        let names = names_in(&[], Some("/run/user/0/wayland-0"));
        assert_eq!(names.first().map(String::as_str), Some("wayland-0"));
        assert!(!names.iter().any(|n| n.contains('/')), "{names:?}");
        // Neither is an empty one, which is what an exported-but-unset
        // variable gives.
        assert_eq!(
            names_in(&[], Some("")).first().map(String::as_str),
            Some("wayland-0")
        );
    }

    /// The lock file is not a socket. Trying it costs a connect that can only
    /// fail, and a listing is the only place its name comes from.
    #[test]
    fn the_lock_files_beside_the_socket_are_not_candidates() {
        let entries = vec![
            "wayland-9".to_string(),
            "wayland-9.lock".to_string(),
            "bus".to_string(),
        ];
        let names = names_in(&entries, None);
        assert!(names.contains(&"wayland-9".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".lock")), "{names:?}");
        assert!(!names.iter().any(|n| n == "bus"), "{names:?}");
    }

    /// An unreadable directory still gets the blind list: that is the case of
    /// a service started before the compositor created its socket, which is
    /// the one this module exists for.
    #[test]
    fn an_empty_listing_still_yields_the_usual_names() {
        let names = names_in(&[], None);
        assert_eq!(names.len(), BLIND_SOCKETS);
        assert_eq!(names[0], "wayland-0");
    }
}
