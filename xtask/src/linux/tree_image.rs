//! Copia de un arbol del anfitrion dentro de una imagen de disco, comun a
//! las dos que el build sabe hacer: btrfs (`btrfs_image`) y EclipseFS
//! (`eclipsefs_image`). Las dos tienen la misma API de driver, asi que el
//! recorrido es uno y cada una solo dice como crear, enlazar y escribir.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// Que se crea en la imagen.
pub(super) enum Node {
    Dir,
    File,
    Fifo,
    BlockDevice,
    CharDevice,
}

/// Lo que el recorrido necesita del sistema de archivos de la imagen.
pub(super) trait TreeFs {
    /// Como se llama en los mensajes ("btrfs", "eclipsefs").
    const NAME: &'static str;
    fn create(&mut self, dir: u64, name: &str, kind: Node, mode: u32, rdev: u64)
        -> Result<u64, String>;
    fn symlink(&mut self, dir: u64, name: &str, target: &[u8]) -> Result<(), String>;
    /// Escritura POSIX: puede quedarse corta si la imagen se llena.
    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, String>;
}

/// Copia el contenido de `dir` (no el propio `dir`) dentro del directorio
/// `dir_ino` de la imagen, en orden alfabetico, con modos y enlaces.
pub(super) fn populate<F: TreeFs>(fs: &mut F, dir_ino: u64, dir: &Path) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {:?}: {}", dir, e))
        .map(|e| e.unwrap())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let path = entry.path();
        // btrfs and EclipseFS store names as bytes, but their API takes `&str`. A
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
                .unwrap_or_else(|e| panic!("symlink {:?}: {}", path, e));
        } else if meta.is_dir() {
            let ino = fs
                .create(dir_ino, name, Node::Dir, mode, 0)
                .unwrap_or_else(|e| panic!("mkdir {:?}: {}", path, e));
            populate(fs, ino, &path);
        } else if meta.is_file() {
            let ino = fs
                .create(dir_ino, name, Node::File, mode, 0)
                .unwrap_or_else(|e| panic!("create {:?}: {}", path, e));
            let data = fs::read(&path).unwrap();
            let mut off = 0u64;
            for chunk in data.chunks(1024 * 1024) {
                // `write` (btrfs and EclipseFS alike) does a POSIX-style PARTIAL write when the
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
                            panic!("write {:?} @ {}: {}", path, off + done as u64, e)
                        });
                    assert!(
                        n > 0,
                        "write {:?} @ {} no avanzo: la imagen {} es demasiado pequena \
                         ({} bytes del fichero sin escribir)",
                        path,
                        F::NAME,
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
                Node::Fifo
            } else if meta.file_type().is_block_device() {
                Node::BlockDevice
            } else if meta.file_type().is_char_device() {
                Node::CharDevice
            } else {
                // Sockets and anything else with no equivalent on disk. Saying so
                // is the point: the installed system will not have it, and this
                // used to be the one branch that left no trace at all.
                eprintln!(
                    "warning: {} no se puede representar en {} y no entra en la imagen",
                    path.display(),
                    F::NAME
                );
                continue;
            };
            fs.create(dir_ino, name, kind, mode, meta.rdev())
                .unwrap_or_else(|e| panic!("mknod {:?}: {}", path, e));
        }
    }
}
