use super::*;

fn svc(name: &str, state: &str, started: Option<u64>) -> Svc {
    Svc {
        name: name.into(),
        kind: "respawn".into(),
        state: state.into(),
        started,
        ..Default::default()
    }
}

fn dir_with(files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("servicios-{}-{}", std::process::id(), files.len()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (n, t) in files {
        std::fs::write(dir.join(n), t).unwrap();
    }
    dir
}

#[test]
fn the_services_are_their_files_joined_with_what_init_says() {
    let dir = dir_with(&[
        ("labwc.service", "# The compositor. See /usr/bin/labwc.\nexec = /usr/bin/labwc\ntype = respawn\ndesktop = labwc\n"),
        ("hunterd.service.off", "# Antivirus daemon\nexec = /usr/local/bin/hunterd\ntype = respawn\n"),
        ("uhda.service", "exec = /bin/x\ntype = respawn\nudriver = hda@0\nlog = /tmp/hda.log\n"),
        ("notas.txt", "x"),
        ("bad name.service", "exec = x\n"),
    ]);
    let all = lee(
        &dir,
        &[
            svc("labwc", "running", Some(10)),
            svc("uhda", "given-up", None),
        ],
        &["labwc"],
    );
    let names: Vec<&str> = all.iter().map(|s| s.nombre.as_str()).collect();
    assert_eq!(names, ["hunterd", "labwc", "uhda"]);
    let (h, l, u) = (&all[0], &all[1], &all[2]);
    assert!(!h.al_arrancar && h.estado() == Estado::NoCargado);
    assert_eq!(h.que, "Antivirus daemon");
    assert!(l.al_arrancar && l.esencial && l.escritorio);
    assert_eq!(l.estado(), Estado::EnMarcha);
    assert_eq!(l.sub(), "The compositor · respawn");
    assert_eq!(u.estado(), Estado::Abandonado);
    assert_eq!(u.sub(), "driver · hda@0");
    assert_eq!(u.registro(), "/tmp/hda.log");
    assert_eq!(h.registro(), "/tmp/hunterd.log");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn turning_a_service_off_renames_its_file_and_an_essential_one_stays() {
    let dir = dir_with(&[
        ("ntpd.service", "exec = ntpd\n"),
        ("seatd.service", "exec = seatd\n"),
    ]);
    let all = lee(&dir, &[], &["seatd"]);
    let (ntpd, seatd) = (&all[0], &all[1]);
    al_arrancar(&dir, ntpd, false).unwrap();
    assert!(dir.join("ntpd.service.off").is_file());
    assert!(!dir.join("ntpd.service").exists());
    let ntpd = &lee(&dir, &[], &[])[0];
    assert!(!ntpd.al_arrancar);
    assert_eq!(ntpd.fichero(&dir), dir.join("ntpd.service.off"));
    al_arrancar(&dir, ntpd, true).unwrap();
    assert!(dir.join("ntpd.service").is_file());
    assert!(al_arrancar(&dir, seatd, false).is_err());
    assert!(dir.join("seatd.service").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_time_running_is_said_short() {
    assert_eq!(desde(None, 100), "—");
    assert_eq!(desde(Some(90), 100), "10 s");
    assert_eq!(desde(Some(0), 600), "10 min");
    assert_eq!(desde(Some(0), 2 * 3600 + 14 * 60), "2 h 14 min");
    assert_eq!(desde(Some(200), 100), "—", "a clock that went back");
}
