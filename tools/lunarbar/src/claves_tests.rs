use super::*;

fn shipped() -> Catalogo {
    Catalogo::de_serie()
}

#[test]
fn every_shipped_block_is_used_and_none_is_left_out() {
    let c = shipped();
    assert!(c.avisos.is_empty(), "{:#?}", c.avisos);
    let ids: Vec<&str> = c.claves.iter().map(|k| k.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "aspecto",
            "idioma",
            "teclado",
            "teclado.idioma",
            "pais",
            "zona",
            "red",
            "monitor",
            "seguridad",
            "sesion",
            "resolucion"
        ]
    );
    let groups: Vec<&str> = c.grupos.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(groups, ["apariencia", "idioma", "fecha", "red", "sistema", "arranque"]);
    for k in &c.claves {
        assert!(!k.en.is_empty(), "[{}] has no English name", k.id);
        assert!(c.grupos.iter().any(|g| g.id == k.grupo && !g.en.is_empty()));
    }
}

#[test]
fn the_shipped_looks_are_the_ones_eclipse_look_accepts() {
    // eclipse-look's `look_ok` (xtask/src/linux/desktop/look.rs) and the
    // panel's own `Look::from_name` must both know every value offered.
    let c = shipped();
    let k = c.claves.iter().find(|k| k.id == "aspecto").unwrap();
    for v in &k.valores {
        assert!(
            crate::look::Look::from_name(&v.v).is_some() || v.v == "gnome",
            "{} is offered but no client knows it",
            v.v
        );
    }
}

#[test]
fn a_bad_block_is_left_out_and_named() {
    let mut c = Catalogo::default();
    c.lee_texto(
        "[a]\ngrupo = g\ntipo = misterio\nfichero = /x\nclave = k\n\
         [b]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\n\
         [c]\ngrupo = g\ntipo = elegir\nfichero = relativo\nclave = k\nvalores = a b\n\
         [d]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\nvalores = a;rm b\n\
         [e]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\nvalores = a b\ndefecto = z\n\
         [f]\ngrupo = g\ntipo = numero\nfichero = /x\nclave = k\nrango = 5..1\n\
         [h]\ngrupo = g\ntipo = si-no\nfichero = /x\nclave = k\nvalores = a b c\n\
         [i]\ngrupo = g\ntipo = abrir\n\
         [ok]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\nvalores = a b\n\
         [ok]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\nvalores = a b\n\
         suelta\n",
        "t",
    );
    assert_eq!(c.claves.len(), 1);
    assert_eq!(c.avisos.len(), 10, "{:#?}", c.avisos);
    assert!(c.avisos.iter().any(|a| a.contains("misterio")));
}

#[test]
fn a_value_is_read_case_blind_and_falls_back_to_the_default() {
    let mut c = Catalogo::default();
    c.lee_texto(
        "[x]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = k\nvalores = a b c\ndefecto = b\n\
         [n]\ngrupo = g\ntipo = numero\nfichero = /x\nclave = k\nrango = 0..10\npaso = 2\ndefecto = 4\n",
        "t",
    );
    let (x, n) = (&c.claves[0], &c.claves[1]);
    assert_eq!(x.normaliza(Some(" C ")), "c");
    assert_eq!(x.normaliza(Some("zz")), "b");
    assert_eq!(x.normaliza(None), "b");
    assert_eq!(x.paso("c", 1), "a");
    assert_eq!(x.paso("a", -1), "c");
    assert_eq!(n.normaliza(Some("99")), "10");
    assert_eq!(n.normaliza(Some("x")), "4");
    assert_eq!(n.paso("4", 1), "6");
    assert_eq!(n.paso("10", 1), "10");
    assert!(n.admite("0") && !n.admite("11") && !n.admite("1;x"));
    assert!(!x.admite("A") && x.admite("a"));
}

#[test]
fn writing_a_key_keeps_every_other_line() {
    let t = "# comentario\nlayout=es\nlanguage = no\n";
    assert_eq!(lee_clave(t, "language").as_deref(), Some("no"));
    assert_eq!(lee_clave(t, "nada"), None);
    assert_eq!(
        pon_clave(t, "language", "yes"),
        "# comentario\nlayout=es\nlanguage=yes\n"
    );
    assert_eq!(pon_clave("es\n", "language", "no"), "es\nlanguage=no\n");
    assert_eq!(pon_clave("", "k", "v"), "k=v\n");
    // A commented-out key is not the key.
    assert_eq!(pon_clave("#k=a\n", "k", "b"), "#k=a\nk=b\n");
    assert_eq!(lee_clave("#k=a\n", "k"), None);
}

#[test]
fn applying_writes_the_file_and_refuses_what_the_data_does_not_allow() {
    let dir = std::env::temp_dir().join(format!("claves-test-{}", std::process::id()));
    let f = dir.join("sub/keyboard");
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!(
            "[t]\ngrupo = g\ntipo = si-no\nfichero = {}\nclave = language\nvalores = yes no\ndefecto = yes\n",
            f.display()
        ),
        "t",
    );
    let k = &c.claves[0];
    assert_eq!(valor_actual(k), "yes");
    aplica(k, "no").unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "language=no\n");
    assert_eq!(valor_actual(k), "no");
    assert!(aplica(k, "no; rm -rf /").is_err());
    assert!(aplica(k, "maybe").is_err());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "language=no\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn applying_through_a_command_passes_the_value_and_reports_a_failure() {
    let dir = std::env::temp_dir().join(format!("claves-cmd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out");
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!(
            "[a]\ngrupo = g\ntipo = elegir\nfichero = /nada\nclave = k\nvalores = uno dos\n\
             aplicar = echo {{}} > {}\n\
             [b]\ngrupo = g\ntipo = elegir\nfichero = /nada\nclave = k\nvalores = uno\n\
             aplicar = echo roto >&2; exit 3 # {{}}\n\
             [l]\ngrupo = g\ntipo = elegir\nfichero = /nada\nclave = k\nvalores = uno dos\n\
             leer = echo DOS\n\
             [m]\ngrupo = g\ntipo = elegir\nfichero = /nada\nclave = k\nvalores = uno dos\n\
             leer = echo usage: nada\n",
            out.display()
        ),
        "t",
    );
    aplica(&c.claves[0], "dos").unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "dos\n");
    let e = aplica(&c.claves[1], "uno").unwrap_err();
    assert!(e.contains("roto"), "{e}");
    assert_eq!(valor_actual(&c.claves[2]), "dos");
    assert_eq!(valor_actual(&c.claves[3]), "uno");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_matches_either_language_without_accents() {
    let c = shipped();
    let name = |v: Vec<usize>| {
        v.into_iter()
            .map(|i| c.claves[i].id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(name(c.busca("distribucion")), ["teclado"]);
    assert_eq!(name(c.busca("KEYBOARD layout")), ["teclado"]);
    assert_eq!(name(c.busca("/etc/eclipse/look")), ["aspecto"]);
    assert!(c.busca("nada de nada").is_empty());
    assert_eq!(c.busca("").len(), c.claves.len());
    assert_eq!(
        name(c.del_grupo(1)),
        ["idioma", "teclado", "teclado.idioma"]
    );
}

#[test]
fn a_one_word_file_is_read_and_written_whole() {
    assert_eq!(lee_palabra("# sesión\n\n  abyss  extra\n").as_deref(), Some("abyss"));
    assert_eq!(lee_palabra("\n# nada\n"), None);
    let dir = std::env::temp_dir().join(format!("claves-palabra-{}", std::process::id()));
    let f = dir.join("desktop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&f, "xorg\n").unwrap();
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!(
            "[s]\ngrupo = g\ntipo = elegir\nfichero = {}\nformato = palabra\nvalores = labwc abyss xorg\n\
             [mal]\ngrupo = g\ntipo = elegir\nfichero = /x\nformato = raro\nvalores = a\n\
             [sin]\ngrupo = g\ntipo = elegir\nfichero = /x\nvalores = a\n",
            f.display()
        ),
        "t",
    );
    assert_eq!(c.claves.len(), 1, "{:?}", c.avisos);
    assert_eq!(c.avisos.len(), 2);
    let k = &c.claves[0];
    assert_eq!(k.origen(), f.display().to_string());
    assert_eq!(valor_actual(k), "xorg");
    aplica(k, "abyss").unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "abyss\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mount_point_is_found_in_proc_mounts() {
    let mounts = "/dev/vda2 / btrfs rw 0 0\n/dev/vda1 /boot/efi vfat rw 0 0\n";
    assert!(montado_en(mounts, Path::new("/boot/efi")));
    assert!(montado_en(mounts, Path::new("/boot/efi/")));
    assert!(!montado_en(mounts, Path::new("/boot")));
    assert!(!montado_en("", Path::new("/boot/efi")));
}

#[test]
fn the_boot_blocks_say_they_are_read_at_boot() {
    let c = shipped();
    for id in ["sesion", "resolucion"] {
        let k = c.claves.iter().find(|k| k.id == id).unwrap();
        assert!(k.reinicio, "{id}");
    }
    let r = c.claves.iter().find(|k| k.id == "resolucion").unwrap();
    assert_eq!(r.montar.as_deref(), Some(Path::new("/boot/efi")));
    assert!(r.admite("1920x1080") && r.admite("auto") && !r.admite("1x1"));
}
