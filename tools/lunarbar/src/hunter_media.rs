//! The desktop's half of hunter's removable-media scan.
//!
//! The kernel notes every filesystem mounted from a device after boot -- a USB
//! stick, an SD card, an external disk -- as a `media-device:` line in
//! `/proc/hunter`, in `state=pending`. It does not walk the tree itself (see
//! `hunter/src/media.rs`); something in userland has to. The bar re-reads the
//! report every few seconds anyway, so it is that something: [`Agent`] sees a
//! pending device and runs `hunterctl scan-media <id>` for it in the
//! background, which walks it through `/proc/hunter_scan` and sends the
//! verdict back to the kernel. Nothing here decides anything: the verdict,
//! the alert (an infected device is a `CRIT` event, which lights the shield)
//! and the lock all live in the kernel.
//!
//! Only as root: `hunterctl` needs it, and a bar running as a user would only
//! start a scan that fails on its first write. `hunterctl scan-media` by hand
//! covers that case, and a console with no desktop at all.

use crate::hunter::{Media, MediaState, Status};
use crate::i18n::Lang;
use std::collections::HashSet;
use std::process::{Child, Command, Stdio};

/// The front end that walks a device.
pub const HUNTERCTL: &str = "/sbin/hunterctl";

/// Where `hunterctl scan-media` leaves one report per device, named by its id.
pub const REPORTS: &str = "/run/hunter/media";

/// Scans running at once. A second stick plugged in while the first is
/// scanned waits for the next free slot: `/proc/hunter_scan` keeps one batch
/// of results for everybody, so two walks at once would read each other's.
const MAX_RUNNING: usize = 1;

/// The devices that need a scan started: pending, while the kernel queues new
/// media, and not already started by this bar.
pub fn to_start(st: &Status, started: &HashSet<u64>) -> Vec<u64> {
    if !st.media_scan {
        return Vec::new();
    }
    st.media
        .iter()
        .filter(|m| m.state == MediaState::Pending && !started.contains(&m.id))
        .map(|m| m.id)
        .collect()
}

/// Starts `hunterctl scan-media` for new devices and reaps the ones that ended.
#[derive(Default)]
pub struct Agent {
    /// Every id a scan was started for, so a device whose scan could not even
    /// start is not retried every tick. Ids are never reused by the kernel.
    started: HashSet<u64>,
    running: Vec<Child>,
}

impl Agent {
    /// One look at the newest status. Returns whether a scan was started.
    pub fn tick(&mut self, st: Option<&Status>) -> bool {
        self.running
            .retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
        let Some(st) = st else { return false };
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return false;
        }
        let mut any = false;
        for id in to_start(st, &self.started) {
            if self.running.len() >= MAX_RUNNING {
                break;
            }
            self.started.insert(id);
            let child = Command::new(HUNTERCTL)
                .arg("scan-media")
                .arg(id.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(c) => {
                    self.running.push(c);
                    any = true;
                }
                Err(e) => eprintln!("lunarbar: {HUNTERCTL} scan-media {id}: {e}"),
            }
        }
        any
    }
}

/// The device the menu talks about: one still being scanned if there is
/// one, else an infected one, else the newest.
pub fn headline(st: &Status) -> Option<&Media> {
    let m = &st.media;
    m.iter()
        .rev()
        .find(|d| matches!(d.state, MediaState::Pending | MediaState::Scanning))
        .or_else(|| m.iter().rev().find(|d| d.state == MediaState::Infected))
        .or_else(|| m.last())
}

/// What one device's scan says, in a few words.
pub fn verdict(lang: Lang, d: &Media) -> String {
    let es = lang == Lang::Es;
    match d.state {
        MediaState::Pending => (if es {
            "esperando escaneo"
        } else {
            "waiting for a scan"
        })
        .into(),
        MediaState::Scanning => (if es { "escaneando..." } else { "scanning..." }).into(),
        MediaState::Clean => {
            if es {
                format!("limpio ({} archivos)", d.files)
            } else {
                format!("clean ({} files)", d.files)
            }
        }
        MediaState::Infected => {
            if es {
                format!("{} amenazas de {} archivos", d.infected, d.files)
            } else {
                format!("{} threats in {} files", d.infected, d.files)
            }
        }
        MediaState::Failed => (if es {
            "el escaneo falló"
        } else {
            "scan failed"
        })
        .into(),
        MediaState::Skipped => (if es { "sin escanear" } else { "not scanned" }).into(),
    }
}

/// The menu's media line: the headline device and its verdict, how many more
/// there are, or that there is none.
pub fn summary(lang: Lang, st: &Status) -> String {
    let Some(d) = headline(st) else {
        return (if lang == Lang::Es {
            "USB: ningún dispositivo montado"
        } else {
            "USB: no device mounted"
        })
        .into();
    };
    let more = st.media.len() - 1;
    let more = if more > 0 {
        format!(" (+{more})")
    } else {
        String::new()
    };
    format!("USB {}: {}{more}", d.target, verdict(lang, d))
}

/// Whether the device needs the user's eye: infected, or the scan failed.
pub fn is_bad(d: &Media) -> bool {
    matches!(d.state, MediaState::Infected | MediaState::Failed)
}

/// The FOUND and ERROR lines of `hunterctl scan-media`'s report on `id`.
pub fn findings(id: u64) -> Vec<String> {
    std::fs::read_to_string(format!("{REPORTS}/{id}"))
        .map(|t| {
            t.lines()
                .filter(|l| l.ends_with(" FOUND") || l.contains(": ERROR "))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: u64, state: MediaState) -> Media {
        Media {
            id,
            state,
            target: format!("/media/d{id}"),
            ..Media::default()
        }
    }

    fn status(media: Vec<Media>) -> Status {
        Status {
            media_known: true,
            media_scan: true,
            media,
            ..Status::default()
        }
    }

    #[test]
    fn only_pending_devices_not_yet_started_are_started() {
        let st = status(vec![
            dev(1, MediaState::Pending),
            dev(2, MediaState::Scanning),
            dev(3, MediaState::Clean),
            dev(4, MediaState::Pending),
        ]);
        let mut started = HashSet::new();
        assert_eq!(to_start(&st, &started), [1, 4]);
        started.insert(1);
        assert_eq!(to_start(&st, &started), [4]);
    }

    #[test]
    fn nothing_is_started_while_the_kernel_does_not_queue_media() {
        let mut st = status(vec![dev(1, MediaState::Pending)]);
        st.media_scan = false;
        assert!(to_start(&st, &HashSet::new()).is_empty());
    }

    #[test]
    fn the_menu_talks_about_the_scan_in_progress_then_the_infected_then_the_newest() {
        let mut st = status(vec![
            dev(1, MediaState::Infected),
            dev(2, MediaState::Scanning),
            dev(3, MediaState::Clean),
        ]);
        assert_eq!(headline(&st).unwrap().id, 2);
        st.media[1].state = MediaState::Clean;
        assert_eq!(headline(&st).unwrap().id, 1);
        st.media[0].state = MediaState::Clean;
        assert_eq!(headline(&st).unwrap().id, 3);
        assert_eq!(headline(&status(vec![])), None);
    }

    #[test]
    fn the_summary_names_the_device_its_verdict_and_how_many_more() {
        let mut d = dev(7, MediaState::Infected);
        d.files = 40;
        d.infected = 2;
        let st = status(vec![dev(6, MediaState::Clean), d]);
        assert_eq!(
            summary(Lang::Es, &st),
            "USB /media/d7: 2 amenazas de 40 archivos (+1)"
        );
        assert_eq!(summary(Lang::En, &status(vec![])), "USB: no device mounted");
        assert_eq!(
            summary(Lang::En, &status(vec![dev(1, MediaState::Pending)])),
            "USB /media/d1: waiting for a scan"
        );
    }
}
