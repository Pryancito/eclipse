//! Generación de imágenes btrfs (rootfs y plantilla de HOME) usando el
//! crate `btrfs` del propio árbol — sin depender de btrfs-progs en el host.

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::{mkfs, Btrfs, FileKind};
use rand::RngCore;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn random_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut u);
    // Bits de versión (4) y variante RFC 4122, como uuidgen.
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

fn mkfs_options(label: &str) -> mkfs::MkfsOptions {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    mkfs::MkfsOptions {
        label: label.into(),
        fsid: random_uuid(),
        chunk_uuid: random_uuid(),
        dev_uuid: random_uuid(),
        subvol_uuid: random_uuid(),
        now: (now.as_secs(), now.subsec_nanos()),
    }
}

/// Crea `image` (de `size` bytes) con un btrfs etiquetado `label` y, si se
/// indica, lo puebla con el contenido de `rootdir`.
pub fn make_btrfs_image(image: &Path, size: u64, label: &str, rootdir: Option<&Path>) {
    let _ = fs::remove_file(image);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image)
        .expect("no se pudo crear la imagen btrfs");
    file.set_len(size)
        .expect("no se pudo dimensionar la imagen");
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(file).unwrap());
    mkfs::format(&*dev, &mkfs_options(label)).expect("mkfs btrfs falló");
    let mut fs = Btrfs::mount(dev, false).expect("no se pudo montar la imagen recién formateada");
    if let Some(dir) = rootdir {
        let root = fs.root_ino();
        populate(&mut fs, root, dir);
    }
    fs.sync().expect("sync de la imagen btrfs falló");
}

fn populate(fs: &mut Btrfs, dir_ino: u64, dir: &Path) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {:?}: {}", dir, e))
        .map(|e| e.unwrap())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let path = entry.path();
        // btrfs stores names as bytes, but this driver's API takes `&str`. A
        // name that is not UTF-8 used to abort with a message that did not say
        // which file, in a build that walks tens of thousands of them.
        let name = name
            .to_str()
            .unwrap_or_else(|| panic!("nombre de archivo no UTF-8: {:?}", path));
        let meta = fs::symlink_metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o7777;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap();
            let target = target
                .as_os_str()
                .to_str()
                .expect("destino de symlink no UTF-8");
            fs.symlink(dir_ino, name, target.as_bytes())
                .unwrap_or_else(|e| panic!("symlink {:?}: {:?}", path, e));
        } else if meta.is_dir() {
            let ino = fs
                .create(dir_ino, name, FileKind::Dir, mode, 0)
                .unwrap_or_else(|e| panic!("mkdir {:?}: {:?}", path, e));
            populate(fs, ino, &path);
        } else if meta.is_file() {
            let ino = fs
                .create(dir_ino, name, FileKind::Regular, mode, 0)
                .unwrap_or_else(|e| panic!("create {:?}: {:?}", path, e));
            let data = fs::read(&path).unwrap();
            let mut off = 0u64;
            for chunk in data.chunks(1024 * 1024) {
                // `Btrfs::write` does a POSIX-style PARTIAL write when the
                // image runs out of space: it returns `Ok(n)` with `n` short of
                // the slice. Advancing by `chunk.len()` regardless loses the
                // tail of this chunk and shifts every later one forward, so the
                // file ends up the right size with the wrong contents and the
                // build says nothing. Write what is left until it is all in, and
                // treat a write that makes no progress as the disk-full it is.
                let mut done = 0usize;
                while done < chunk.len() {
                    let n = fs
                        .write(ino, off + done as u64, &chunk[done..])
                        .unwrap_or_else(|e| {
                            panic!("write {:?} @ {}: {:?}", path, off + done as u64, e)
                        });
                    assert!(
                        n > 0,
                        "write {:?} @ {} no avanzo: la imagen btrfs es demasiado pequena \
                         ({} bytes del fichero sin escribir)",
                        path,
                        off + done as u64,
                        data.len() as u64 - off - done as u64,
                    );
                    done += n;
                }
                off += chunk.len() as u64;
            }
        } else {
            // Nodos de dispositivo / FIFOs en el rootfs de build: poco
            // habituales; se preservan con su rdev.
            let kind = if meta.file_type().is_fifo() {
                FileKind::Fifo
            } else if meta.file_type().is_block_device() {
                FileKind::BlockDevice
            } else if meta.file_type().is_char_device() {
                FileKind::CharDevice
            } else {
                // Sockets and anything else with no btrfs equivalent. Saying so
                // is the point: the installed system will not have it, and this
                // used to be the one branch that left no trace at all.
                eprintln!(
                    "warning: {} no se puede representar en btrfs y no entra en la imagen",
                    path.display()
                );
                continue;
            };
            fs.create(dir_ino, name, kind, mode, meta.rdev())
                .unwrap_or_else(|e| panic!("mknod {:?}: {:?}", path, e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Un directorio propio por test: la suite corre en paralelo dentro de un
    /// mismo proceso.
    fn escenario(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "xtask-btrfs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// Monta la imagen con el driver btrfs **del propio arbol**, que es lo que
    /// permite comprobar el resultado sin btrfs-progs en el anfitrion.
    fn montar(image: &Path) -> Btrfs {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(image)
            .expect("no se pudo abrir la imagen");
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(file).unwrap());
        Btrfs::mount(dev, true).expect("la imagen recien escrita no se puede montar")
    }

    fn leer_todo(fs: &mut Btrfs, ino: u64) -> Vec<u8> {
        let size = fs.stat(ino).unwrap().size as usize;
        let mut buf = vec![0u8; size];
        let n = fs.read(ino, 0, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    /// Un patron que depende de la POSICION, para que un trozo escrito en el
    /// offset equivocado no pueda coincidir por casualidad.
    fn patron(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7 + 13) % 251) as u8).collect()
    }

    /// La cobertura de verdad de este fichero, y la que faltaba: construir la
    /// imagen y **volver a leerla**, sin depender de ninguna herramienta del
    /// anfitrion. El unico test que habia antes empezaba con un `btrfs version`
    /// y volvia sin hacer nada si no estaba instalado --y no lo esta ni aqui ni
    /// en la CI, cuyo propio comentario dice que esos tests "skip"--, asi que
    /// `cargo test` daba verde sobre el codigo que construye `rootfs.btrfs`, o
    /// sea la raiz real del sistema instalado, sin haberlo ejecutado nunca.
    #[test]
    fn la_imagen_se_puede_leer_con_el_driver_del_propio_arbol() {
        let base = escenario("ida-y-vuelta");
        let src = base.join("rootfs");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::create_dir_all(src.join("etc")).unwrap();
        let busybox = patron(2 * 1024 * 1024);
        fs::write(src.join("bin/busybox"), &busybox).unwrap();
        fs::set_permissions(src.join("bin/busybox"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("busybox", src.join("bin/sh")).unwrap();
        fs::write(src.join("etc/fstab"), b"# fstab\n").unwrap();

        let img = base.join("rootfs.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "ECLIPSE", Some(&src));

        let mut fs2 = montar(&img);
        assert_eq!(fs2.label(), "ECLIPSE");
        let root = fs2.root_ino();

        let bin = fs2.lookup(root, "bin").expect("falta /bin");
        let ino = fs2.lookup(bin, "busybox").expect("falta /bin/busybox");
        assert_eq!(
            leer_todo(&mut fs2, ino),
            busybox,
            "el contenido no coincide"
        );

        let etc = fs2.lookup(root, "etc").expect("falta /etc");
        let ino = fs2.lookup(etc, "fstab").unwrap();
        assert_eq!(leer_todo(&mut fs2, ino), b"# fstab\n");
        let _ = fs::remove_dir_all(&base);
    }

    /// Los permisos son los del rootfs, incluidos los bits de setuid: un binario
    /// que llega sin su `x` no arranca, y uno que llega sin su setuid arranca sin
    /// privilegios, que es peor de diagnosticar.
    #[test]
    fn los_modos_sobreviven_incluido_el_setuid() {
        let base = escenario("modos");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        for (name, mode) in [
            ("ejecutable", 0o755u32),
            ("privado", 0o600),
            ("consetuid", 0o4755),
            ("consticky", 0o1777),
        ] {
            fs::write(src.join(name), b"x").unwrap();
            fs::set_permissions(src.join(name), fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::create_dir_all(src.join("dir700")).unwrap();
        fs::set_permissions(src.join("dir700"), fs::Permissions::from_mode(0o700)).unwrap();

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        for (name, mode) in [
            ("ejecutable", 0o755u32),
            ("privado", 0o600),
            ("consetuid", 0o4755),
            ("consticky", 0o1777),
            ("dir700", 0o700),
        ] {
            let ino = fs2
                .lookup(root, name)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let st = fs2.stat(ino).unwrap();
            assert_eq!(st.mode & 0o7777, mode, "el modo de {name}");
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// Un enlace tiene que llegar como enlace y con su destino tal cual. Copiar
    /// el fichero apuntado meteria un busybox entero por applet, y reescribir el
    /// destino dejaria cientos de enlaces rotos en el sistema instalado.
    #[test]
    fn un_enlace_llega_como_enlace_y_con_su_destino() {
        let base = escenario("enlaces");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("busybox"), b"bb").unwrap();
        std::os::unix::fs::symlink("busybox", src.join("relativo")).unwrap();
        std::os::unix::fs::symlink("../../bin/busybox", src.join("suberelativo")).unwrap();
        std::os::unix::fs::symlink("/bin/busybox", src.join("absoluto")).unwrap();
        std::os::unix::fs::symlink("no-existe", src.join("roto")).unwrap();

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        for (name, destino) in [
            ("relativo", "busybox"),
            ("suberelativo", "../../bin/busybox"),
            ("absoluto", "/bin/busybox"),
            // Un enlace roto en el anfitrion sigue siendo un enlace: resolverlo
            // aqui lo dejaria fuera de la imagen.
            ("roto", "no-existe"),
        ] {
            let ino = fs2
                .lookup(root, name)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(fs2.stat(ino).unwrap().kind, FileKind::Symlink, "{name}");
            assert_eq!(
                String::from_utf8(fs2.read_link(ino).unwrap()).unwrap(),
                destino,
                "el destino de {name}"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// El contenido se escribe en trozos de 1 MiB, asi que el offset del
    /// siguiente trozo es lo unico que puede estar mal ahi -- y un trozo en el
    /// offset equivocado da un fichero del tamano correcto con el contenido
    /// desplazado, que es un fichero que pasa cualquier comprobacion de tamano.
    /// El patron depende de la posicion justamente para que no pueda colar.
    #[test]
    fn un_fichero_de_varios_trozos_se_escribe_en_orden() {
        let base = escenario("trozos");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        let tamanos = [
            1024 * 1024 - 1,     // justo por debajo de un trozo
            1024 * 1024,         // exactamente un trozo
            1024 * 1024 + 1,     // el segundo trozo con un solo byte
            2 * 1024 * 1024,     // dos trozos exactos
            2 * 1024 * 1024 + 7, // tres trozos, el ultimo corto
        ];
        for len in tamanos {
            fs::write(src.join(format!("f{len}")), patron(len)).unwrap();
        }

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        for len in tamanos {
            let name = format!("f{len}");
            let ino = fs2.lookup(root, &name).unwrap();
            assert_eq!(
                fs2.stat(ino).unwrap().size,
                len as u64,
                "el tamano de {name}"
            );
            assert_eq!(
                leer_todo(&mut fs2, ino),
                patron(len),
                "el contenido de {name}"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// Un fichero vacio no escribe ningun trozo --`chunks` de un slice vacio no
    /// da ninguno--, asi que existe solo si el `create` basta por si mismo. Y
    /// los hay en el rootfs: `/etc/apk/world` sale vacio y `apk` lo lee.
    #[test]
    fn un_fichero_vacio_y_un_directorio_vacio_existen() {
        let base = escenario("vacios");
        let src = base.join("rootfs");
        fs::create_dir_all(src.join("etc/apk")).unwrap();
        fs::write(src.join("etc/apk/world"), b"").unwrap();
        fs::create_dir_all(src.join("proc")).unwrap();

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        let etc = fs2.lookup(root, "etc").unwrap();
        let apk = fs2.lookup(etc, "apk").unwrap();
        let ino = fs2.lookup(apk, "world").expect("falta /etc/apk/world");
        let st = fs2.stat(ino).unwrap();
        assert_eq!(st.kind, FileKind::Regular);
        assert_eq!(st.size, 0);
        assert!(leer_todo(&mut fs2, ino).is_empty());

        let proc = fs2
            .lookup(root, "proc")
            .expect("falta el punto de montaje /proc");
        assert_eq!(fs2.stat(proc).unwrap().kind, FileKind::Dir);
        assert!(fs2
            .readdir(proc)
            .unwrap()
            .iter()
            .all(|e| e.name == "." || e.name == ".."));
        let _ = fs::remove_dir_all(&base);
    }

    /// El anidamiento llega entero: `populate` es recursivo y el rootfs tiene
    /// rutas de varios niveles que el sistema instalado busca por su ruta
    /// completa (`/usr/share/udhcpc/default.script`, por ejemplo).
    #[test]
    fn el_anidamiento_profundo_llega_entero() {
        let base = escenario("anidado");
        let src = base.join("rootfs");
        let hondo = src.join("usr/share/udhcpc/sub/mas/todavia");
        fs::create_dir_all(&hondo).unwrap();
        fs::write(hondo.join("default.script"), b"#!/bin/sh\n").unwrap();

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let mut ino = fs2.root_ino();
        for parte in ["usr", "share", "udhcpc", "sub", "mas", "todavia"] {
            ino = fs2
                .lookup(ino, parte)
                .unwrap_or_else(|e| panic!("falta {parte}: {e:?}"));
        }
        let script = fs2.lookup(ino, "default.script").unwrap();
        assert_eq!(leer_todo(&mut fs2, script), b"#!/bin/sh\n");
        let _ = fs::remove_dir_all(&base);
    }

    /// Las entradas se ordenan por nombre antes de escribirlas, y el inodo se
    /// asigna en el orden en que se crean: o sea que en la imagen **el orden de
    /// los inodos es el orden alfabetico de los nombres**, y eso es lo que hace
    /// que dos builds del mismo rootfs den la misma imagen. Sin ordenar, el
    /// reparto de inodos lo decide el orden en que el anfitrion devuelva el
    /// directorio, que no es estable ni entre dos maquinas ni entre dos builds.
    #[test]
    fn los_inodos_se_reparten_en_orden_alfabetico() {
        let base = escenario("orden");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        // Creados a proposito en un orden que no es el alfabetico.
        for n in ["zeta", "alfa", "omega", "beta", "gamma", "delta"] {
            fs::write(src.join(n), n.as_bytes()).unwrap();
        }

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        let mut entradas: Vec<(String, u64)> = fs2
            .readdir(root)
            .unwrap()
            .into_iter()
            .filter(|e| e.name != "." && e.name != "..")
            .map(|e| (e.name, e.ino))
            .collect();
        assert_eq!(entradas.len(), 6);
        entradas.sort_by(|a, b| a.1.cmp(&b.1));
        let por_inodo: Vec<&str> = entradas.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            por_inodo,
            ["alfa", "beta", "delta", "gamma", "omega", "zeta"],
            "los inodos no se repartieron en orden alfabetico, o sea que la \
             imagen depende del orden del directorio del anfitrion"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// La plantilla de HOME se escribe cruda en la particion y el kernel la
    /// expande, asi que tiene que salir montable y vacia: un `rootdir` de `None`
    /// no puede acabar copiando nada.
    #[test]
    fn la_plantilla_de_home_sale_vacia_y_montable() {
        let base = escenario("home");
        let img = base.join("home.btrfs");
        make_btrfs_image(&img, 32 * 1024 * 1024, "HOME", None);

        let mut fs2 = montar(&img);
        assert_eq!(fs2.label(), "HOME");
        let root = fs2.root_ino();
        let entradas: Vec<_> = fs2
            .readdir(root)
            .unwrap()
            .into_iter()
            .filter(|e| e.name != "." && e.name != "..")
            .collect();
        assert!(
            entradas.is_empty(),
            "la plantilla no esta vacia: {entradas:?}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// Una imagen que no da para el arbol tiene que **fallar**, no salir a
    /// medias: un `rootfs.btrfs.gz` con un fichero truncado se instala sin una
    /// palabra y falla en el arranque del sistema instalado, a un mundo de
    /// distancia de la causa.
    #[test]
    fn una_imagen_demasiado_pequena_falla_y_no_calla() {
        let base = escenario("pequena");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("grande"), patron(24 * 1024 * 1024)).unwrap();

        let img = base.join("i.btrfs");
        let salio =
            std::panic::catch_unwind(|| make_btrfs_image(&img, 20 * 1024 * 1024, "S", Some(&src)));
        assert!(
            salio.is_err(),
            "24 MiB de contenido entraron en una imagen de 20 MiB, o sea que algo se quedo por el camino"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// Los UUID llevan los bits de version y variante de RFC 4122, como los de
    /// `uuidgen`: `blkid` y `btrfs filesystem show` los leen, y un fsid con la
    /// version mal puesta se lee como otra cosa o como basura.
    #[test]
    fn los_uuid_llevan_la_version_4_y_la_variante_rfc_4122() {
        for _ in 0..64 {
            let u = random_uuid();
            assert_eq!(u[6] & 0xf0, 0x40, "el nibble de version no es 4: {u:02x?}");
            assert_eq!(
                u[8] & 0xc0,
                0x80,
                "los bits de variante no son 10: {u:02x?}"
            );
        }
        assert_ne!(
            random_uuid(),
            random_uuid(),
            "dos UUID seguidos iguales: no se esta generando nada"
        );
    }

    /// Un socket no tiene equivalente en btrfs, asi que se salta -- pero se
    /// salta **diciendolo**: el sistema instalado no lo va a tener, y este era
    /// el unico camino que no dejaba ni rastro. Y saltarselo no puede tumbar el
    /// build: un socket olvidado en el rootfs no es motivo para no hacer imagen.
    #[test]
    fn un_socket_se_queda_fuera_y_no_tumba_el_build() {
        let base = escenario("socket");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("normal"), b"x").unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(src.join("socket")).unwrap();
        assert!(
            fs::symlink_metadata(src.join("socket"))
                .unwrap()
                .file_type()
                .is_socket(),
            "el escenario no creo un socket"
        );

        let img = base.join("i.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "S", Some(&src));

        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        assert!(
            fs2.lookup(root, "normal").is_ok(),
            "el resto del arbol si entra"
        );
        assert!(
            fs2.lookup(root, "socket").is_err(),
            "un socket no se puede representar en btrfs"
        );
        drop(_listener);
        let _ = fs::remove_dir_all(&base);
    }

    /// Validacion cruzada contra btrfs-progs, que es lo unico que dice que la
    /// imagen es btrfs de verdad y no solo algo que este driver sabe releer.
    ///
    /// **Se salta cuando la herramienta no esta, y no esta ni aqui ni en la CI**,
    /// asi que la cobertura de este fichero la llevan los tests de arriba. Con
    /// `ECLIPSE_REQUIRE_BTRFS_PROGS=1` el salto pasa a ser un fallo, para poder
    /// exigirlo donde si este instalada.
    #[test]
    fn la_validacion_contra_btrfs_progs() {
        let disponible = Command::new("btrfs")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !disponible {
            let exigido = std::env::var("ECLIPSE_REQUIRE_BTRFS_PROGS").as_deref() == Ok("1");
            assert!(
                !exigido,
                "ECLIPSE_REQUIRE_BTRFS_PROGS=1 y btrfs-progs no esta instalado"
            );
            println!("btrfs-progs no disponible; la validacion cruzada no se ejecuta");
            return;
        }
        let base = escenario("progs");
        let src = base.join("rootfs");
        fs::create_dir_all(src.join("bin")).unwrap();
        let busybox = patron(2 * 1024 * 1024);
        fs::write(src.join("bin/busybox"), &busybox).unwrap();
        fs::set_permissions(src.join("bin/busybox"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("busybox", src.join("bin/sh")).unwrap();
        fs::write(src.join("bin/fstab"), b"# fstab\n").unwrap();

        let img = base.join("rootfs.btrfs");
        make_btrfs_image(&img, 96 * 1024 * 1024, "ECLIPSE", Some(&src));

        let out = Command::new("btrfs")
            .args(["check", "--force"])
            .arg(&img)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "btrfs check fallo:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        let restored = base.join("restored");
        fs::create_dir_all(&restored).unwrap();
        let out = Command::new("btrfs")
            .args(["restore", "-v"])
            .arg(&img)
            .arg(&restored)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read(restored.join("bin/busybox")).unwrap(), busybox);
        assert_eq!(fs::read(restored.join("bin/fstab")).unwrap(), b"# fstab\n");

        let home = base.join("home.btrfs");
        make_btrfs_image(&home, 32 * 1024 * 1024, "HOME", None);
        let out = Command::new("btrfs")
            .args(["check", "--force"])
            .arg(&home)
            .output()
            .unwrap();
        assert!(out.status.success());
        let _ = fs::remove_dir_all(&base);
    }
}
