use super::*;

fn shipped() -> Catalogo {
    Catalogo::de_serie()
}

#[test]
fn a_services_page_keeps_its_essentials_and_an_openers_page_its_programs() {
    let c = Catalogo::de_serie();
    let s = c.claves.iter().find(|k| k.id == "servicios").unwrap();
    assert_eq!(s.tipo, Tipo::Servicios);
    assert!(s.valores.iter().any(|v| v.v == "seatd"));
    let a = c.claves.iter().find(|k| k.id == "aperturas").unwrap();
    assert_eq!(a.tipo, Tipo::Aperturas);
    let progs: Vec<&str> = a.valores.iter().map(|v| v.v.as_str()).collect();
    assert_eq!(progs, ["eclipse-edit", "eclipse-view", "eclipse-firefox", "wavplay", "mpg123"]);
    assert_eq!(a.etiqueta("eclipse-view", true), "Viewer");
    // Every program the page offers is one the image's table already uses.
    for p in progs {
        assert!(crate::open::BUILTIN.iter().any(|(_, c)| c.starts_with(p)), "{p}");
    }
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
            "acento",
            "reloj",
            "idioma",
            "teclado",
            "teclado.idioma",
            "pais",
            "zona",
            "red",
            "monitor",
            "seguridad",
            "sesion",
            "resolucion",
            "efectos",
            "texto",
            "fondo.animado",
            "fondo.fps",
            "fondo.reloj",
            "fondo.fecha",
            "fondo.lecturas",
            "lectura.carga",
            "lectura.cpu",
            "lectura.memoria",
            "lectura.red",
            "lectura.disco",
            "lectura.temperatura",
            "lectura.en_marcha",
            "lectura.procesos",
            "barra.alto",
            "barra.escritorios",
            "barra.titulo",
            "barra.cpu",
            "barra.memoria",
            "barra.red",
            "barra.temperatura",
            "barra.bateria",
            "barra.disco",
            "barra.en_marcha",
            "barra.carga",
            "arranque.log",
            "arranque.smp",
            "arranque.renderer",
            "arranque.nvidia",
            "arranque.nvidia.render",
            "arranque.nvkms",
            "arranque.medida",
            "dispositivos",
            "servicios",
            "aperturas"
        ]
    );
    let groups: Vec<&str> = c.grupos.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(groups, ["apariencia", "idioma", "fecha", "red", "sistema", "arranque", "escritorio", "fondo", "barra", "avanzado", "dispositivos", "servicios", "aperturas"]);
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
    assert_eq!(lee_palabra("# sesión\n\n  xorg  extra\n").as_deref(), Some("xorg"));
    assert_eq!(lee_palabra("\n# nada\n"), None);
    let dir = std::env::temp_dir().join(format!("claves-palabra-{}", std::process::id()));
    let f = dir.join("desktop");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&f, "xorg\n").unwrap();
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!(
            "[s]\ngrupo = g\ntipo = elegir\nfichero = {}\nformato = palabra\nvalores = labwc xorg\n\
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
    aplica(k, "labwc").unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "labwc\n");
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

#[test]
fn the_desktop_blocks_go_through_eclipse_entorno_and_read_its_file() {
    let c = shipped();
    let mut desk = Vec::new();
    for (id, n) in [("escritorio", 2), ("fondo", 13), ("barra", 11)] {
        let g = c.grupos.iter().position(|g| g.id == id).unwrap();
        assert_eq!(c.del_grupo(g).len(), n, "{id}");
        desk.extend(c.del_grupo(g));
    }
    for &i in &desk {
        let k = &c.claves[i];
        let var = k.clave.as_deref().unwrap();
        assert_eq!(k.fichero.as_deref(), Some(Path::new("/etc/eclipse/entorno")), "{}", k.id);
        assert_eq!(
            k.aplicar.as_deref(),
            Some(format!("/usr/local/bin/eclipse-entorno {var} {{}}").as_str()),
            "{}",
            k.id
        );
        // What the panel reads back is what the program reads: the first line.
        let d = k.defecto.as_deref().unwrap();
        let text = pon_clave("# x\n", var, d);
        assert_eq!(lunartext::entorno::de_texto(&text, var).as_deref(), Some(d));
    }
    let alto = c.claves.iter().find(|k| k.id == "barra.alto").unwrap();
    assert!(alto.admite("auto") && alto.admite("44") && !alto.admite("100"));
}

fn vals(s: &str) -> Vec<Valor> {
    s.split_whitespace()
        .map(|v| Valor {
            v: v.into(),
            es: v.into(),
            en: v.into(),
        })
        .collect()
}

#[test]
fn a_boot_key_is_read_first_spelling_first_and_written_in_its_place() {
    let line = "LOG=error:TERM=xterm-256color:nvidia.cuda:log=info:ROOT=/dev/sda2";
    let log = Arranque::Clave("LOG".into());
    let v = vals("error warn info");
    assert_eq!(lee_arranque(line, &log, &v).as_deref(), Some("error"));
    assert_eq!(
        pon_arranque(line, &log, &v, "debug"),
        "LOG=debug:TERM=xterm-256color:nvidia.cuda:ROOT=/dev/sda2"
    );
    assert_eq!(
        pon_arranque(line, &log, &v, "auto"),
        "TERM=xterm-256color:nvidia.cuda:ROOT=/dev/sda2"
    );
    let smp = Arranque::Clave("smp".into());
    assert_eq!(lee_arranque(line, &smp, &v), None);
    assert_eq!(pon_arranque(line, &smp, &v, "off"), format!("{line}:smp=off"));
    assert_eq!(pon_arranque("", &smp, &v, "4"), "smp=4");
    // `ROOTPROC=/bin/busybox?sh` and the like go through untouched.
    let odd = "ROOTPROC=/bin/busybox?sh:LOG=warn";
    assert_eq!(pon_arranque(odd, &log, &v, "info"), "ROOTPROC=/bin/busybox?sh:LOG=info");
}

#[test]
fn a_boot_flag_is_one_of_its_values_or_none() {
    let f = Arranque::Bandera;
    let v = vals("auto nvidia.wlr_vulkan nvidia.wlr_pixman");
    let line = "LOG=error:nvidia.wlr_pixman:nvidia.cuda:nvidia.wlr_vulkan";
    assert_eq!(lee_arranque(line, &f, &v).as_deref(), Some("nvidia.wlr_pixman"));
    assert_eq!(
        pon_arranque(line, &f, &v, "nvidia.wlr_vulkan"),
        "LOG=error:nvidia.wlr_vulkan:nvidia.cuda"
    );
    assert_eq!(pon_arranque(line, &f, &v, "auto"), "LOG=error:nvidia.cuda");
    // `nvidia.cuda` is not one of them, and neither is a longer token.
    assert_eq!(lee_arranque("nvidia.cuda:nvidia.wlr_vulkanx", &f, &v), None);
}

#[test]
fn rboot_conf_is_read_and_written_on_its_last_line() {
    let t = "# cmdline=x\ncmdline=LOG=info\nresolution=auto\ncmdline=LOG=warn:smp=2\n";
    assert_eq!(lee_ultima(t, "cmdline").as_deref(), Some("LOG=warn:smp=2"));
    assert_eq!(
        pon_ultima(t, "cmdline", "LOG=error"),
        "# cmdline=x\ncmdline=LOG=info\nresolution=auto\ncmdline=LOG=error\n"
    );
    assert_eq!(pon_ultima("a=1\n", "cmdline", "LOG=error"), "a=1\ncmdline=LOG=error\n");
}

#[test]
fn applying_a_boot_option_edits_the_command_line_and_nothing_else() {
    let dir = std::env::temp_dir().join(format!("claves-boot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("rboot.conf");
    let conf = "# rboot\nkernel_path=\\EFI\\zCore\\zcore.elf\nresolution=auto\n\
                cmdline=LOG=error:TERM=xterm-256color:nvidia.cuda:ROOT=__ECLIPSE_CMDROOTDEV\n";
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!(
            "[smp]\ngrupo = g\ntipo = elegir\nfichero = {f}\nclave = cmdline\narranque = smp\n\
             valores = auto off 2\ndefecto = auto\n\
             [gpu]\ngrupo = g\ntipo = si-no\nfichero = {f}\nclave = cmdline\narranque = bandera\n\
             valores = nvidia.nvkms auto\ndefecto = auto\n",
            f = f.display()
        ),
        "t",
    );
    assert!(c.avisos.is_empty(), "{:?}", c.avisos);
    let (smp, gpu) = (&c.claves[0], &c.claves[1]);
    // No file: nothing is made in its place.
    assert!(aplica(smp, "off").is_err());
    assert!(!f.exists());
    std::fs::write(&f, conf).unwrap();
    assert_eq!(valor_actual(smp), "auto");
    aplica(smp, "off").unwrap();
    aplica(gpu, "nvidia.nvkms").unwrap();
    assert_eq!(valor_actual(smp), "off");
    assert_eq!(valor_actual(gpu), "nvidia.nvkms");
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        conf.replace(
            "ROOT=__ECLIPSE_CMDROOTDEV",
            "ROOT=__ECLIPSE_CMDROOTDEV:smp=off:nvidia.nvkms"
        )
    );
    aplica(smp, "auto").unwrap();
    aplica(gpu, "auto").unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), conf);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_boot_block_must_say_where_its_command_line_is() {
    let mut c = Catalogo::default();
    c.lee_texto(
        "[a]\ngrupo = g\ntipo = elegir\nfichero = /x\narranque = LOG\nvalores = info\n\
         [b]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = cmdline\narranque = LOG\nvalores = info\naplicar = echo {}\n\
         [c]\ngrupo = g\ntipo = elegir\nfichero = /x\nclave = cmdline\narranque = LOG:smp\nvalores = info\n\
         [d]\ngrupo = g\ntipo = numero\nrango = 1..4\nfichero = /x\nclave = cmdline\narranque = smp\n",
        "t",
    );
    assert!(c.claves.is_empty());
    assert_eq!(c.avisos.len(), 4, "{:?}", c.avisos);
}

#[test]
fn the_advanced_boot_blocks_edit_rboot_conf_on_the_esp() {
    let c = shipped();
    let g = c.grupos.iter().position(|g| g.id == "avanzado").unwrap();
    for i in c.del_grupo(g) {
        let k = &c.claves[i];
        assert!(k.reinicio, "{}", k.id);
        assert_eq!(k.montar.as_deref(), Some(Path::new("/boot/efi")), "{}", k.id);
        assert_eq!(
            k.fichero.as_deref(),
            Some(Path::new("/boot/efi/EFI/Boot/rboot.conf")),
            "{}",
            k.id
        );
    }
    let r = c.claves.iter().find(|k| k.id == "arranque.renderer").unwrap();
    assert_eq!(r.origen(), "rboot.conf · cmdline · renderer");
}

#[test]
fn a_volume_fstab_cannot_mount_is_waited_for_once_not_once_per_setting() {
    let at = Path::new("/no/hay/tal/volumen");
    assert!(asegura_montaje(at).is_err());
    let t = std::time::Instant::now();
    for _ in 0..10 {
        assert!(asegura_montaje(at).is_err());
    }
    assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
    // What the panel reads first does not wait at all.
    let c = Catalogo::de_serie();
    let t = std::time::Instant::now();
    for k in &c.claves {
        let _ = valor_leido(k);
    }
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
}
