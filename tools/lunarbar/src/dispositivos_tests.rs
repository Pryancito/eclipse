use super::*;
use crate::claves::Catalogo;

const IDS: &str = "\
# pci.ids sample
10de  NVIDIA Corporation
\t2684  AD102 [GeForce RTX 4090]
\t\t10de 167c  subsystem, skipped
8086  Intel Corporation
\t7ae0  Alder Lake-S PCH USB 3.2 Gen 2x2 XHCI Controller
\t15f3  Ethernet Controller I225-V
14c3  MEDIATEK Corp.
C 01  Mass storage controller
\t08  Non-Volatile memory controller
\t\t02  NVM Express
C 02  Network controller
\t80  Network controller
C 0c  Serial bus controller
\t03  USB controller
\t\t30  XHCI
C 06  Bridge
\t00  Host bridge
";

fn pci(bdf: &str, v: u16, d: u16, class: u32) -> Pci {
    Pci {
        bdf: bdf.into(),
        vendor: v,
        device: d,
        class,
    }
}

fn drivers() -> Vec<Driver> {
    Catalogo::de_serie().drivers
}

#[test]
fn pci_ids_gives_the_names_lspci_prints() {
    let n = Nombres::lee(IDS);
    assert_eq!(n.fabricante(0x10de), Some("NVIDIA Corporation"));
    assert_eq!(n.modelo(0x10de, 0x2684), Some("AD102 [GeForce RTX 4090]"));
    assert_eq!(n.modelo(0x10de, 0x167c), None, "a subsystem is not a device");
    assert_eq!(n.clase(0x010802), Some("NVM Express"));
    assert_eq!(n.clase(0x028000), Some("Network controller"));
    assert_eq!(n.clase(0x0c0330), Some("XHCI"));
    assert_eq!(n.clase(0x060400), Some("Bridge"), "an unknown subclass falls to the base");
    assert_eq!(n.clase(0xff0000), None);
    assert!(Nombres::lee("").vacio());
}

#[test]
fn every_shipped_driver_block_is_used() {
    let c = Catalogo::de_serie();
    assert!(c.avisos.is_empty(), "{:#?}", c.avisos);
    assert!(c.drivers.len() >= 10);
    assert!(c
        .claves
        .iter()
        .any(|k| k.tipo == crate::claves::Tipo::Dispositivos));
}

#[test]
fn a_driver_block_matches_by_ids_then_by_maker_and_class() {
    let d = drivers();
    let who = |p: &Pci| d.iter().find(|x| x.casa(p)).map(|x| x.id.as_str());
    assert_eq!(who(&pci("x", 0x8086, 0x100e, 0x020000)), Some("e1000"));
    assert_eq!(who(&pci("x", 0x8086, 0x15f3, 0x020000)), Some("e1000e"));
    assert_eq!(who(&pci("x", 0x144d, 0xa80a, 0x010802)), Some("nvme"));
    assert_eq!(who(&pci("x", 0x8086, 0x7ae2, 0x010601)), Some("ahci"));
    assert_eq!(who(&pci("x", 0x8086, 0x7ae2, 0x010400)), None, "RAID mode is no AHCI");
    assert_eq!(who(&pci("x", 0x10de, 0x2684, 0x030000)), Some("nvidia"));
    assert_eq!(who(&pci("x", 0x10de, 0x22ba, 0x040300)), Some("hda"), "a GPU's HDMI audio");
    assert_eq!(who(&pci("x", 0x1234, 0x1111, 0x030000)), Some("gop"));
    assert_eq!(who(&pci("x", 0x8086, 0x7a84, 0x060100)), Some("puente"));
    assert_eq!(who(&pci("x", 0x14c3, 0x0616, 0x028000)), None);
}

#[test]
fn a_bad_driver_block_is_left_out_and_named() {
    let mut c = Catalogo::default();
    c.lee_texto(
        "[driver:a]\nes = x\n\
         [driver:b]\nids = 10de\n\
         [driver:c]\nclase = 123\n\
         [driver:d]\nclase = 03\nudriver = x@0\n\
         [driver:ok]\nclase = 03\n\
         [driver:ok]\nclase = 04\n",
        "t",
    );
    assert_eq!(c.drivers.len(), 1);
    assert_eq!(c.avisos.len(), 5, "{:#?}", c.avisos);
}

#[test]
fn the_state_comes_from_proc_udrivers_and_the_services() {
    let d = drivers();
    let n = Nombres::lee(IDS);
    let bus = vec![
        pci("0000:00:14.0", 0x8086, 0x7ae0, 0x0c0330),
        pci("0000:01:00.0", 0x10de, 0x2684, 0x030000),
        pci("0000:02:00.0", 0x144d, 0xa80a, 0x010802),
        pci("0000:03:00.0", 0x144d, 0xa80a, 0x010802),
        pci("0000:04:00.0", 0x14c3, 0x0616, 0x028000),
        pci("0000:05:00.0", 0x1af4, 0x1041, 0x020000),
        pci("0000:00:00.0", 0x8086, 0x4660, 0x060000),
    ];
    let svcs = vec![Svc {
        name: "unvme".into(),
        state: "running".into(),
        udriver: Some("nvme@0".into()),
        ..Default::default()
    }];
    let f = calcula(&bus, &d, &n, "nvme@0\nnvme@1\n", &svcs, "LOG=error:udriver=xhci@0");
    let st: Vec<Estado> = f.iter().map(|x| x.estado).collect();
    assert_eq!(
        st,
        [
            Estado::Nucleo,
            Estado::Nucleo,
            Estado::Usuario,
            Estado::Parado,
            Estado::SinDriver,
            Estado::Libre,
            Estado::Sistema,
        ]
    );
    assert_eq!(f[0].nombre, "Alder Lake-S PCH USB 3.2 Gen 2x2 XHCI Controller");
    assert_eq!(f[0].fabricante.as_deref(), Some("Intel Corporation"));
    assert_eq!(f[3].item.as_deref(), Some("nvme@1"), "counted per kind in bus order");
    assert_eq!(f[4].nombre, "Network controller", "no model: the class");
    assert!(f[0].pedido, "named in udriver=, the kernel has not handed it yet");
    // What the row's action does.
    assert_eq!(f[0].accion(), Some(false), "named: give it back");
    assert_eq!(f[1].accion(), None, "nvidia has no userspace driver");
    assert_eq!(f[2].accion(), None, "already in userspace unasked");
    assert_eq!(f[4].accion(), None);
    assert_eq!(f[5].accion(), Some(true));
    assert!(informe(&f, &d).contains("04:00.0 14c3:0616 [028000] MEDIATEK Corp. Network controller\tdriver=-\tSIN DRIVER"));
}

#[test]
fn a_device_is_added_to_and_taken_from_udriver() {
    let line = "LOG=error:udriver=nvme@0:ROOT=/dev/sda2";
    assert_eq!(con_udriver(line, "xhci@0", true), "LOG=error:udriver=nvme@0,xhci@0:ROOT=/dev/sda2");
    assert_eq!(con_udriver(line, "nvme@0", true), line, "already there");
    assert_eq!(con_udriver(line, "nvme@0", false), "LOG=error:ROOT=/dev/sda2");
    assert_eq!(con_udriver("LOG=error", "hda@0", true), "LOG=error:udriver=hda@0");
    assert_eq!(lista_udriver("udriver=a@0,,b@1"), ["a@0", "b@1"]);
}

#[test]
fn handing_a_device_over_edits_rboot_conf_and_refuses_a_bad_item() {
    let dir = std::env::temp_dir().join(format!("disp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("rboot.conf");
    std::fs::write(&f, "resolution=auto\ncmdline=LOG=error:ROOT=__X\n").unwrap();
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!(
            "[d]\ngrupo = g\ntipo = dispositivos\nfichero = {}\nclave = cmdline\n",
            f.display()
        ),
        "t",
    );
    let k = &c.claves[0];
    cambia(k, "xhci@0", true).unwrap();
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "resolution=auto\ncmdline=LOG=error:ROOT=__X:udriver=xhci@0\n"
    );
    assert!(cambia(k, "xhci@0:LOG=trace", true).is_err());
    assert!(cambia(k, "xhci", true).is_err());
    cambia(k, "xhci@0", false).unwrap();
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "resolution=auto\ncmdline=LOG=error:ROOT=__X\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_installed_driver_takes_its_device_before_the_shipped_blocks() {
    let mut c = Catalogo::de_serie();
    // The shipped `virtio` block takes every 1af4 device; an installed
    // driver for one of them goes first.
    c.lee_texto(
        "[driver:udrv-1af4-1050]\nes = udrv-1af4-1050\nids = 1af4:1050\nnucleo = no\n\
         udriver = pci-1af4-1050\ninstalado = si\n\
         [driver:udrv-14c3-0616]\nes = udrv-14c3-0616\nids = 14c3:0616\nnucleo = no\n\
         udriver = pci-14c3-0616\ninstalado = si\n",
        "t",
    );
    assert!(c.avisos.is_empty(), "{:?}", c.avisos);
    let bus = [
        pci("0000:00:02.0", 0x1af4, 0x1050, 0x038000),
        pci("0000:05:00.0", 0x14c3, 0x0616, 0x028000),
        pci("0000:06:00.0", 0x14c3, 0x0616, 0x028000),
    ];
    let filas = calcula(
        &bus,
        &c.drivers,
        &Nombres::lee(IDS),
        "pci-14c3-0616@0\n",
        &[],
        "udriver=pci-14c3-0616@0,pci-1af4-1050@0",
    );
    let es = |f: &Fila| c.drivers[f.driver.unwrap()].es.clone();
    assert_eq!(es(&filas[0]), "udrv-1af4-1050");
    assert!(c.drivers[filas[0].driver.unwrap()].instalado);
    // Counted per identity, as the kernel counts them.
    assert_eq!(filas[1].item.as_deref(), Some("pci-14c3-0616@0"));
    assert_eq!(filas[2].item.as_deref(), Some("pci-14c3-0616@1"));
    assert_eq!(filas[1].estado, Estado::Parado, "left to userspace, no service");
    assert!(filas[1].pedido);
    // Not handed over yet (the next boot does): pending, can go back.
    assert_eq!(filas[0].estado, Estado::Libre);
    assert_eq!(filas[0].accion(), Some(false));
    assert!(!Catalogo::de_serie().drivers.iter().any(|d| d.instalado));
}

#[test]
fn the_driver_files_found_are_drv_files_with_their_device_first() {
    let dir = std::env::temp_dir().join(format!("disp-cand-{}", std::process::id()));
    let dl = dir.join("Descargas");
    std::fs::create_dir_all(&dl).unwrap();
    for p in [
        dir.join("otro.drv"),
        dl.join("mt7922-14c3-0616.DRV"),
        dir.join("udrv-14c3-0616"),
        dir.join("notas.txt"),
    ] {
        std::fs::write(p, "x").unwrap();
    }
    std::fs::create_dir_all(dir.join("carpeta.drv")).unwrap();
    let found = candidatos(&dirs_candidatos(&dir), (0x14c3, 0x0616));
    assert_eq!(found, [dl.join("mt7922-14c3-0616.DRV"), dir.join("otro.drv")]);
    assert!(candidatos(&[dir.join("nada")], (0, 0)).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_driver_file_names_its_device() {
    let ids = |n: &str| ids_de_nombre(Path::new(n));
    assert_eq!(ids("/x/mt7922-14c3-0616.drv"), Some((0x14c3, 0x0616)));
    assert_eq!(ids("14C3_0616.drv"), Some((0x14c3, 0x0616)));
    assert_eq!(ids("wifi-8086:7aa3-v2.drv"), Some((0x8086, 0x7aa3)));
    assert_eq!(ids("mt7922.drv"), None);
    assert_eq!(ids("a14c3-06161.drv"), None, "not four digits each");
    assert!(es_drv(Path::new("a.DRV")) && !es_drv(Path::new("a.drv.txt")));
}

#[test]
fn installing_runs_the_script_then_names_the_device_in_rboot_conf() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("disp-inst-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("rboot.conf");
    std::fs::write(&f, "cmdline=LOG=error\n").unwrap();
    let log = dir.join("log");
    let script = dir.join("eclipse-udriver");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\n[ \"$3\" = bad ] && {{ echo 'eclipse-udriver: no' >&2; exit 2; }}\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = Catalogo::default();
    c.lee_texto(
        &format!("[d]\ngrupo = g\ntipo = dispositivos\nfichero = {}\nclave = cmdline\n", f.display()),
        "t",
    );
    let k = &c.claves[0];
    let s = script.to_str().unwrap();
    let wifi = pci("0000:05:00.0", 0x14c3, 0x0616, 0x028000);
    assert_eq!(servicio(&wifi), "udrv-14c3-0616");
    instala_con(s, k, &wifi, Some(Path::new("/home/x/mt7922.drv"))).unwrap();
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "cmdline=LOG=error:udriver=pci-14c3-0616@0\n"
    );
    instala_con(s, k, &wifi, None).unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "cmdline=LOG=error\n");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "instalar 14c3 0616 /home/x/mt7922.drv\nquitar 14c3 0616\n"
    );
    // A script that fails leaves rboot.conf alone and says why.
    let bad = pci("0000:07:00.0", 0x14c3, 0x0bad, 0x028000);
    std::fs::write(&script, "#!/bin/sh\necho 'eclipse-udriver: no' >&2\nexit 2\n").unwrap();
    assert_eq!(
        instala_con(s, k, &bad, Some(Path::new("/x"))),
        Err("eclipse-udriver: no".into())
    );
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "cmdline=LOG=error\n");
    let _ = std::fs::remove_dir_all(&dir);
}
