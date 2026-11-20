//! The installed root, when its disk comes from a driver this init starts.
//!
//! The kernel chooses `/` before any process runs. Every disk controller is
//! driven by a process now, and those processes are services of this init,
//! so on an installed machine (`ROOT=/dev/nvme0n1p2`) the kernel finds no
//! such disk, keeps the boot medium (the initramfs on the ESP) as `/` and
//! says so in `/proc/switch_root`: the device it awaits, one line.
//!
//! Then this init, running from the boot medium, starts only the disk
//! drivers, waits for the device, writes it back to `/proc/switch_root` (the
//! kernel makes it `/` for every process here) and executes the installed
//! system's init. That one is handed the drivers it already has running in
//! [`ADOPTED_ENV`] and adopts them instead of starting them again: a second
//! driver for the same controller would only fail at its claim.
//!
//! When the device does not come in time, or the kernel refuses it, the boot
//! carries on from the boot medium, as it did before, with the reason on the
//! console.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::{log, start_service, timed, Service};

/// Where the kernel says which root it awaits, and takes it.
const SWITCH_ROOT: &str = "/proc/switch_root";

/// The services of the boot medium that the installed init takes over, as
/// `name=pid` separated by commas.
pub const ADOPTED_ENV: &str = "ECLIPSE_INIT_ADOPTED";

/// How long the disk may take to appear: a real NVMe controller can take
/// seconds to come ready after its reset, and the driver has to start first.
const DEVICE_WAIT: Duration = Duration::from_secs(30);

/// The `udriver =` kinds whose devices are disks a root can be on.
const DISK_KINDS: &[&str] = &["virtio-blk", "nvme", "ahci", "xhci"];

/// The device the kernel awaits as `/`, if it does.
pub fn awaited() -> Option<String> {
    awaited_in(&fs::read_to_string(SWITCH_ROOT).ok()?)
}

fn awaited_in(text: &str) -> Option<String> {
    let dev = text.lines().next()?.trim();
    dev.starts_with("/dev/").then(|| dev.to_string())
}

/// Whether a service drives a disk: its `udriver =`, as the state file
/// listed it (`nvme@0`, `ahci/2`), is of a disk kind.
fn drives_a_disk(udriver: Option<&str>) -> bool {
    udriver
        .and_then(|d| d.split(['@', '/']).next())
        .is_some_and(|kind| DISK_KINDS.contains(&kind))
}

/// Start the disk drivers, wait for `dev` and make it `/`; on success this
/// executes the installed system's init and does not return. On failure it
/// returns, and the boot carries on from the boot medium with whatever was
/// started here still running (the caller skips what has a pid).
pub fn switch(dev: &str, services: &mut BTreeMap<String, Service>) {
    log(&format!(
        "root: the kernel awaits {dev}, whose disk a driver here serves; starting the disk drivers"
    ));
    for svc in services.values_mut() {
        if drives_a_disk(svc.udriver.as_deref()) && svc.pid.is_none() {
            start_service(svc);
        }
    }
    let node = Path::new(dev);
    let came = timed(format!("root: wait for {dev}"), || {
        let t0 = Instant::now();
        while !node.exists() {
            if t0.elapsed() >= DEVICE_WAIT {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    });
    if !came {
        log(&format!(
            "root: {dev} did not appear in {DEVICE_WAIT:?}; staying on the boot medium (is its driver's log in /tmp?)"
        ));
        return;
    }
    // The disk's node comes a moment before its partitions' on some
    // drivers: ENOENT is retried for a little while.
    let t0 = Instant::now();
    let asked = loop {
        match fs::write(SWITCH_ROOT, dev) {
            Ok(()) => break Ok(()),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && t0.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = asked {
        log(&format!(
            "root: the kernel did not take {dev} as / ({e}); staying on the boot medium"
        ));
        return;
    }
    log(&format!("root: / is {dev}; handing over to its init"));
    let _ = std::env::set_current_dir("/");
    std::env::set_var(ADOPTED_ENV, adopted_list(services));
    let mut argv: Vec<String> = std::env::args().collect();
    match argv.first() {
        Some(first) if first.starts_with('/') => {}
        _ => {
            if argv.is_empty() {
                argv.push(String::new());
            }
            argv[0] = String::from("/sbin/init");
        }
    }
    super::exec_argv(&argv);
    // Not there or not runnable on the new root: this init goes on, from
    // the new root, with the services it already has.
    log(&format!(
        "root: could not execute {} on {dev}; carrying on with this init",
        argv[0]
    ));
    std::env::remove_var(ADOPTED_ENV);
}

fn adopted_list(services: &BTreeMap<String, Service>) -> String {
    services
        .iter()
        .filter_map(|(name, s)| s.pid.map(|pid| format!("{name}={pid}")))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_adopted(text: &str) -> Vec<(String, i32)> {
    text.split(',')
        .filter_map(|item| {
            let (name, pid) = item.split_once('=')?;
            let pid: i32 = pid.trim().parse().ok().filter(|p| *p > 1)?;
            Some((name.trim().to_string(), pid))
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// Take over the services the boot medium's init started before it handed
/// over (see the module's text). A service this root does not have is still
/// our child, and is reaped like any other.
pub fn adopt(services: &mut BTreeMap<String, Service>) {
    let Ok(text) = std::env::var(ADOPTED_ENV) else {
        return;
    };
    std::env::remove_var(ADOPTED_ENV);
    for (name, pid) in parse_adopted(&text) {
        match services.get_mut(&name) {
            Some(svc) => {
                svc.pid = Some(pid);
                svc.started_at = Some(Instant::now());
                svc.total_starts += 1;
                log(&format!("respawn: {name} (adopted, pid {pid})"));
            }
            None => log(&format!(
                "root: {name} (pid {pid}) was started by the boot medium and this root has no such service"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_device_line_is_awaited() {
        assert_eq!(awaited_in("/dev/nvme0n1p2\n"), Some("/dev/nvme0n1p2".into()));
        assert_eq!(awaited_in(""), None);
        assert_eq!(awaited_in("\n"), None);
        assert_eq!(awaited_in("rootfs\n"), None);
    }

    #[test]
    fn disk_drivers_are_told_by_their_kind() {
        assert!(drives_a_disk(Some("nvme@0")));
        assert!(drives_a_disk(Some("ahci/2")));
        assert!(drives_a_disk(Some("virtio-blk@1")));
        assert!(drives_a_disk(Some("xhci@0")));
        assert!(!drives_a_disk(Some("virtio-net@0")));
        assert!(!drives_a_disk(Some("hda@0")));
        assert!(!drives_a_disk(None));
    }

    #[test]
    fn the_adopted_list_reads_back() {
        assert_eq!(
            parse_adopted("unvme=12,uahci=13"),
            vec![("unvme".into(), 12), ("uahci".into(), 13)]
        );
        // Never pid 1 or garbage.
        assert_eq!(parse_adopted("a=1,b=x,=5,c"), vec![]);
        assert_eq!(parse_adopted(""), vec![]);
    }
}
