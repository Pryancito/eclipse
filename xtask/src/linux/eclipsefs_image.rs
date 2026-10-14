//! Imagenes EclipseFS (rootfs y plantilla de HOME) con el crate `eclipsefs`
//! del propio arbol, como `btrfs_image` hace las btrfs. Es lo que se instala
//! por defecto (ver `DiskFs`); `ECLIPSE_ROOTFS_FS=btrfs` vuelve a btrfs.

use super::tree_image::{populate, Node, TreeFs};
use eclipsefs::device::{BlockDevice, FileDevice};
use eclipsefs::{mkfs, EclipseFS, FileKind};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn mkfs_options(label: &str) -> mkfs::MkfsOptions {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let uuid = super::btrfs_image::random_uuid;
    mkfs::MkfsOptions {
        label: label.into(),
        fsid: uuid(),
        chunk_uuid: uuid(),
        dev_uuid: uuid(),
        subvol_uuid: uuid(),
        now: (now.as_secs(), now.subsec_nanos()),
    }
}

fn clock() -> (u64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs(), now.subsec_nanos())
}

/// Crea `image` (de `size` bytes) con un EclipseFS etiquetado `label` y, si
/// se indica, lo puebla con el contenido de `rootdir`. Al montarlo, el
/// kernel lo agranda hasta el tamano de la particion.
pub fn make_eclipsefs_image(image: &Path, size: u64, label: &str, rootdir: Option<&Path>) {
    let _ = fs::remove_file(image);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(image)
        .expect("no se pudo crear la imagen EclipseFS");
    file.set_len(size)
        .expect("no se pudo dimensionar la imagen");
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(file).unwrap());
    mkfs::format(&*dev, &mkfs_options(label)).expect("mkfs EclipseFS fallo");
    let mut fs =
        EclipseFS::mount(dev, false).expect("no se pudo montar la imagen recien formateada");
    fs.set_clock(clock);
    if let Some(dir) = rootdir {
        let root = fs.root_ino();
        populate(&mut fs, root, dir);
    }
    fs.sync().expect("sync de la imagen EclipseFS fallo");
    let report = fs
        .check()
        .expect("la imagen EclipseFS recien escrita no pasa su propio fsck");
    println!(
        "EclipseFS {}: {} inodos, {} directorios, {} bloques",
        image.display(),
        report.inodes,
        report.directories,
        report.blocks_used
    );
}

impl TreeFs for EclipseFS {
    const NAME: &'static str = "EclipseFS";

    fn create(
        &mut self,
        dir: u64,
        name: &str,
        kind: Node,
        mode: u32,
        rdev: u64,
    ) -> Result<u64, String> {
        let kind = match kind {
            Node::Dir => FileKind::Dir,
            Node::File => FileKind::Regular,
            Node::Fifo => FileKind::Fifo,
            Node::BlockDevice => FileKind::BlockDevice,
            Node::CharDevice => FileKind::CharDevice,
        };
        EclipseFS::create(self, dir, name, kind, mode, rdev).map_err(|e| format!("{e:?}"))
    }

    fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<(), String> {
        EclipseFS::symlink(self, dir, name, target)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, String> {
        EclipseFS::write(self, ino, offset, data).map_err(|e| format!("{e:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    /// Un directorio propio por test: la suite corre en paralelo dentro de un
    /// mismo proceso.
    fn escenario(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "xtask-eclipsefs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// Monta la imagen con el driver EclipseFS del propio arbol, el mismo
    /// codigo que la monta en el kernel.
    fn montar(image: &Path) -> EclipseFS {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(image)
            .expect("no se pudo abrir la imagen");
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(file).unwrap());
        EclipseFS::mount(dev, true).expect("la imagen recien escrita no se puede montar")
    }

    fn leer_todo(fs: &mut EclipseFS, ino: u64) -> Vec<u8> {
        let size = fs.stat(ino).unwrap().size as usize;
        let mut buf = vec![0u8; size];
        let mut n = 0;
        while n < size {
            n += fs.read(ino, n as u64, &mut buf[n..]).unwrap();
        }
        buf
    }

    fn patron(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7 + 13) % 251) as u8).collect()
    }

    /// Construir la imagen y **volver a leerla**: contenido, enlaces, modos
    /// (setuid incluido) y directorios vacios, y que pase su fsck.
    #[test]
    fn la_imagen_se_lee_entera_con_el_driver_del_propio_arbol() {
        let base = escenario("ida-y-vuelta");
        let src = base.join("rootfs");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::create_dir_all(src.join("etc")).unwrap();
        fs::create_dir_all(src.join("var/empty")).unwrap();
        let busybox = patron(3 * 1024 * 1024 + 17);
        fs::write(src.join("bin/busybox"), &busybox).unwrap();
        fs::set_permissions(src.join("bin/busybox"), fs::Permissions::from_mode(0o4755)).unwrap();
        std::os::unix::fs::symlink("busybox", src.join("bin/sh")).unwrap();
        fs::write(src.join("etc/fstab"), b"# fstab\n").unwrap();
        fs::write(src.join("etc/empty"), b"").unwrap();

        let img = base.join("rootfs.eclipsefs");
        make_eclipsefs_image(&img, 96 * 1024 * 1024, "ECLIPSE", Some(&src));

        let mut fs2 = montar(&img);
        assert_eq!(fs2.label(), "ECLIPSE");
        assert_eq!(fs2.format_version(), 4, "el formato nuevo, copy-on-write");
        let root = fs2.root_ino();
        let bin = fs2.lookup(root, "bin").expect("falta /bin");
        let ino = fs2.lookup(bin, "busybox").expect("falta /bin/busybox");
        assert_eq!(leer_todo(&mut fs2, ino), busybox);
        assert_eq!(fs2.stat(ino).unwrap().mode & 0o7777, 0o4755);
        let sh = fs2.lookup(bin, "sh").unwrap();
        assert_eq!(fs2.stat(sh).unwrap().kind, FileKind::Symlink);
        assert_eq!(fs2.read_link(sh).unwrap(), b"busybox");
        let etc = fs2.lookup(root, "etc").unwrap();
        let fstab = fs2.lookup(etc, "fstab").unwrap();
        assert_eq!(leer_todo(&mut fs2, fstab), b"# fstab\n");
        let empty = fs2.lookup(etc, "empty").unwrap();
        assert_eq!(fs2.stat(empty).unwrap().size, 0);
        let var = fs2.lookup(root, "var").unwrap();
        let vacio = fs2.lookup(var, "empty").unwrap();
        assert!(fs2.readdir(vacio).unwrap().is_empty());
        assert!(fs2.scrub().unwrap().is_clean());
        let _ = fs::remove_dir_all(&base);
    }

    /// La plantilla de HOME sale vacia y montable, y el kernel la agranda a
    /// la particion: aqui, a un fichero mas grande que la imagen.
    #[test]
    fn la_plantilla_de_home_sale_vacia_y_crece_hasta_la_particion() {
        let base = escenario("home");
        let img = base.join("home.eclipsefs");
        make_eclipsefs_image(&img, 32 * 1024 * 1024, "HOME", None);
        {
            let mut fs2 = montar(&img);
            assert_eq!(fs2.label(), "HOME");
            let root = fs2.root_ino();
            assert!(fs2.readdir(root).unwrap().is_empty());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img)
            .unwrap();
        file.set_len(128 * 1024 * 1024).unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(file).unwrap());
        let mut fs2 = EclipseFS::mount(dev, false).unwrap();
        assert!(fs2.grow_to_device().unwrap());
        assert!(fs2.fsinfo().total_bytes > 100 * 1024 * 1024);
        let _ = fs::remove_dir_all(&base);
    }

    /// Una imagen que no da para el arbol tiene que fallar, no salir a medias.
    #[test]
    fn una_imagen_demasiado_pequena_falla_y_no_calla() {
        let base = escenario("pequena");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("grande"), patron(24 * 1024 * 1024)).unwrap();
        let img = base.join("i.eclipsefs");
        let salio = std::panic::catch_unwind(|| {
            make_eclipsefs_image(&img, 16 * 1024 * 1024, "S", Some(&src))
        });
        assert!(salio.is_err(), "24 MiB entraron en una imagen de 16 MiB");
        let _ = fs::remove_dir_all(&base);
    }

    /// Un socket no se puede representar en disco: se salta diciendolo, y el
    /// resto del arbol entra.
    #[test]
    fn un_socket_se_queda_fuera_y_no_tumba_el_build() {
        let base = escenario("socket");
        let src = base.join("rootfs");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("normal"), b"x").unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(src.join("socket")).unwrap();
        assert!(fs::symlink_metadata(src.join("socket"))
            .unwrap()
            .file_type()
            .is_socket());
        let img = base.join("i.eclipsefs");
        make_eclipsefs_image(&img, 32 * 1024 * 1024, "S", Some(&src));
        let mut fs2 = montar(&img);
        let root = fs2.root_ino();
        assert!(fs2.lookup(root, "normal").is_ok());
        assert!(fs2.lookup(root, "socket").is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
