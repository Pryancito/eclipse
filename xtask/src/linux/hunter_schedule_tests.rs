//! hunter's scheduled scans: what `install_hunter` lays down for them, the
//! shipped `/etc/hunter/schedule`, and `hunterctl`'s slot arithmetic, RUN
//! under `sh` rather than read -- an off-by-one there is a scan that never
//! happens, and nothing on the machine would say so.

use super::*;

const HUNTERCTL: &str = "tools/hunterctl/hunterctl";

/// The default schedule is what every new image scans with: each job line
/// has to be one `hunterctl schedule` and the scheduler both understand.
#[test]
fn the_shipped_schedule_is_well_formed() {
    let text = fs::read_to_string(PROJECT_DIR.join("tools/hunterctl/schedule")).unwrap();
    let jobs: Vec<Vec<&str>> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().collect())
        .collect();
    let names: Vec<&str> = jobs.iter().map(|f| f[0]).collect();
    assert_eq!(names, ["quick", "full"], "{text}");
    let days = [
        "daily", "off", "mon", "tue", "wed", "thu", "fri", "sat", "sun",
    ];
    for f in &jobs {
        assert!(f.len() >= 5, "job without paths: {f:?}");
        assert!(days.contains(&f[1]), "when: {f:?}");
        let (h, m) = f[2].split_once(':').expect("HH:MM");
        assert!(h.len() == 2 && m.len() == 2, "HH:MM: {f:?}");
        assert!(h.parse::<u32>().unwrap() < 24 && m.parse::<u32>().unwrap() < 60);
        assert!(["report", "quarantine"].contains(&f[3]), "action: {f:?}");
        assert!(f[4..].iter().all(|p| p.starts_with('/')), "paths: {f:?}");
    }
}

#[test]
fn install_hunter_ships_the_schedule_and_its_service() {
    let dir = std::env::temp_dir().join(format!("eclipse-hunter-sched-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    LinuxRootfs::install_hunter(&dir);
    assert!(dir.join("etc/hunter/schedule").is_file());
    let svc =
        fs::read_to_string(dir.join("etc/eclipse/services/hunter-scheduler.service")).unwrap();
    let lines: Vec<&str> = svc.lines().filter(|l| !l.starts_with('#')).collect();
    assert!(lines.contains(&"exec = /sbin/hunterctl scheduler"), "{svc}");
    assert!(lines.contains(&"type = respawn"), "{svc}");
    // An operator's schedule survives a rebuild.
    fs::write(
        dir.join("etc/hunter/schedule"),
        "quick off 12:00 report /bin\n",
    )
    .unwrap();
    LinuxRootfs::install_hunter(&dir);
    let kept = fs::read_to_string(dir.join("etc/hunter/schedule")).unwrap();
    assert_eq!(kept, "quick off 12:00 report /bin\n");
    let _ = fs::remove_dir_all(&dir);
}

/// `next_slot` lifted out of hunterctl and run: daily and weekly, a last run
/// exactly on the slot, one second before it, and a timezone on each side
/// of UTC. 1793232000 is 2026-10-29T00:00Z, a Thursday.
#[test]
fn next_slot_is_the_first_slot_after_the_last_run() {
    let script = fs::read_to_string(PROJECT_DIR.join(HUNTERCTL)).unwrap();
    let start = script.find("tz_offset() {").expect("tz_offset");
    let end = script.find("# `YYYY-MM-DD HH:MM`").expect("when_str");
    let day = 86_400u64;
    let t0 = 1_793_232_000u64;
    let cases: &[(u64, &str, &str, i64, u64)] = &[
        (t0, "daily", "12:00", 0, t0 + 12 * 3600),
        (t0 + 12 * 3600, "daily", "12:00", 0, t0 + day + 12 * 3600),
        (t0 + 12 * 3600 - 1, "daily", "12:00", 0, t0 + 12 * 3600),
        (t0, "sun", "13:00", 0, t0 + 3 * day + 13 * 3600),
        (t0, "thu", "00:00", 0, t0 + 7 * day),
        (t0, "thu", "00:01", 0, t0 + 60),
        (t0, "mon", "08:30", 0, t0 + 4 * day + 8 * 3600 + 1800),
        // UTC+2: local 12:00 is 10:00Z.
        (t0, "daily", "12:00", 7200, t0 + 10 * 3600),
        // UTC-5: local 01:00 is 06:00Z, the same day.
        (t0, "daily", "01:00", -18000, t0 + 6 * 3600),
    ];
    let mut sh = script[start..end].to_string();
    for (last, when, at, off, _) in cases {
        sh.push_str(&format!("next_slot {last} {when} {at} {off}\n"));
    }
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&sh)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got: Vec<u64> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    let want: Vec<u64> = cases.iter().map(|c| c.4).collect();
    assert_eq!(got, want);
}
