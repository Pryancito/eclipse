//! What a server says about itself, for whoever looks: one file per
//! server under [`DIR`], `key=value` lines.
//!
//! ```text
//! /run/eclipse-servers/driver.virtio-net@0
//! pid=231
//! program=udrv-virtio-net
//! device=virtio-net@0
//! serves=kernel
//! mac=52:54:00:12:34:56
//! ```
//!
//! A driver writes its file once it serves (`driver.<kind>@<N>`), a stack
//! once it has a card and each time it starts another driver
//! (`stack.<kind>@<N>`). The file is written beside its place and renamed
//! over it, so a reader never sees half. The next process of the same
//! server writes over it; a file whose `pid` is not alive is one that has
//! gone. Best effort: a `/run` that cannot be written costs the monitor its
//! view, never a server. `lunarmon` reads these.

use std::fmt::Write as _;
use std::fs;

/// Where the files go.
pub const DIR: &str = "/run/eclipse-servers";

/// The text of a server's file: `pid` first, then `fields` in order. A
/// value loses any line break, which would start another key.
pub fn render(pid: u32, fields: &[(&str, &str)]) -> String {
    let mut s = format!("pid={pid}\n");
    for (k, v) in fields {
        let v: String = v.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        let _ = writeln!(s, "{k}={}", v.trim());
    }
    s
}

/// Publish `fields` as this process's file `name` (`driver.virtio-blk@1`).
pub fn publish(name: &str, fields: &[(&str, &str)]) {
    let text = render(std::process::id(), fields);
    let _ = fs::create_dir_all(DIR);
    let path = format!("{DIR}/{name}");
    let tmp = format!("{path}.tmp");
    if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// `52:54:00:12:34:56`.
pub fn mac_text(mac: [u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_the_pid_then_the_fields_one_per_line() {
        let t = render(7, &[("device", "virtio-net@0"), ("mac", "52:54:00:12:34:56")]);
        assert_eq!(t, "pid=7\ndevice=virtio-net@0\nmac=52:54:00:12:34:56\n");
    }

    #[test]
    fn a_value_cannot_start_another_key() {
        let t = render(1, &[("addr", "10.0.9.15/24\nserves=kernel")]);
        assert_eq!(t.lines().count(), 2);
    }

    #[test]
    fn a_mac_is_six_hex_pairs() {
        assert_eq!(mac_text([0x52, 0x54, 0, 0x12, 0x34, 0xab]), "52:54:00:12:34:ab");
    }
}
