//! Interfaz de Sistema de Ficheros Virtual (VFS) para Eclipse OS

use spin::Mutex;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct StatInfo {
    pub inode: u32,
    pub size: u64,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub nlink: u32,
}

#[derive(Debug)]
pub enum VfsError {
    FileNotFound,
    FileExists,
    NotADirectory,
    NotAFile,
    InvalidPath,
    PermissionDenied,
    IoError(String),
    DeviceError(String),
    InvalidFs(String),
    InvalidOperation,
    InvalidArgument,
    NoSpaceLeft,
}

pub trait FileSystem: Send + Sync {
    fn unmount(&mut self) -> Result<(), VfsError>;
    fn read(&self, inode: u32, offset: u64, buffer: &mut [u8]) -> Result<usize, VfsError>;
    fn write(&mut self, inode: u32, offset: u64, data: &[u8]) -> Result<usize, VfsError>;
    fn stat(&self, inode: u32) -> Result<StatInfo, VfsError>;
    fn readdir(&self, inode: u32) -> Result<Vec<String>, VfsError>;
    fn truncate(&mut self, inode: u32, new_size: u64) -> Result<(), VfsError>;
    fn rmdir(&mut self, parent_inode: u32, name: &str) -> Result<(), VfsError>;
    fn rename(&mut self, parent_inode: u32, old_name: &str, new_parent_inode: u32, new_name: &str) -> Result<(), VfsError>;
    fn unlink(&mut self, parent_inode: u32, name: &str) -> Result<(), VfsError>;
    fn chmod(&mut self, inode: u32, mode: u16) -> Result<(), VfsError>;
    fn chown(&mut self, inode: u32, uid: u32, gid: u32) -> Result<(), VfsError>;

    /// Resolver una ruta absoluta a un inode. Implementaciones que no soporten rutas pueden
    /// retornar `VfsError::InvalidOperation`.
    fn resolve_path(&self, _path: &str) -> Result<u32, VfsError> {
        Err(VfsError::InvalidOperation)
    }

    /// Listar un directorio especificado por ruta absoluta.
    fn readdir_path(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let inode = self.resolve_path(path)?;
        self.readdir(inode)
    }

    /// Leer por completo un archivo especificado por ruta absoluta y devolver su contenido.
    fn read_file_path(&self, _path: &str) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::InvalidOperation)
    }
}

struct MountPoint {
    fs: Arc<Mutex<Box<dyn FileSystem>>>,
    path: String,
}

pub struct Vfs {
    mounts: Vec<MountPoint>,
}

impl Vfs {
    pub fn new() -> Self {
        Vfs { mounts: Vec::new() }
    }

    pub fn get_mount(&self, path: &str) -> Option<Arc<Mutex<Box<dyn FileSystem>>>> {
        self.mounts.iter().find(|mp| mp.path == path).map(|mp| mp.fs.clone())
    }

    pub fn mount(&mut self, path: &str, fs: Box<dyn FileSystem>) {
        crate::debug::serial_write_str(&alloc::format!("VFS: montando {}\n", path));
        if let Some(_existing) = self.get_mount(path) {
            if let Some(pos) = self.mounts.iter().position(|mp| mp.path == path) {
                self.mounts[pos] = MountPoint {
                    fs: Arc::new(Mutex::new(fs)),
                    path: path.to_string(),
                };
                crate::debug::serial_write_str("VFS: reemplazo de montaje existente\n");
                return;
            }
        }
        self.mounts.push(MountPoint {
            fs: Arc::new(Mutex::new(fs)),
            path: path.to_string(),
        });
        crate::debug::serial_write_str("VFS: montaje añadido\n");
    }
    
    /// 🚀 MÉTODO REDOXFS-STYLE: Montar sin allocación dinámica
    /// Usa el Box que ya fue creado por new_static()
    pub fn mount_static(&mut self, path: &str, fs: Box<dyn FileSystem>) {
        crate::debug::serial_write_str(&alloc::format!("VFS: montando {} (RedoxFS-style, sin allocación dinámica)\n", path));
        if let Some(_existing) = self.get_mount(path) {
            if let Some(pos) = self.mounts.iter().position(|mp| mp.path == path) {
                self.mounts[pos] = MountPoint {
                    fs: Arc::new(Mutex::new(fs)),
                    path: path.to_string(),
                };
                crate::debug::serial_write_str("VFS: reemplazo de montaje existente (RedoxFS-style)\n");
                return;
            }
        }
        self.mounts.push(MountPoint {
            fs: Arc::new(Mutex::new(fs)),
            path: path.to_string(),
        });
        crate::debug::serial_write_str("VFS: montaje añadido (RedoxFS-style)\n");
    }

    pub fn get_root_fs(&self) -> Option<Arc<Mutex<Box<dyn FileSystem>>>> {
        self.get_mount("/")
    }

    pub fn debug_list_mounts(&self) {
        crate::debug::serial_write_str("VFS: listado de montajes:\n");
        for mp in &self.mounts {
            crate::debug::serial_write_str(&alloc::format!("  - {}\n", mp.path));
        }
    }
}

static VFS_INSTANCE: Mutex<Option<Vfs>> = Mutex::new(None);

pub fn init_vfs() {
    let mut guard = VFS_INSTANCE.lock();
    if guard.is_none() {
        *guard = Some(Vfs::new());
    }
}

pub fn get_vfs() -> spin::MutexGuard<'static, Option<Vfs>> {
    VFS_INSTANCE.lock()
}

pub fn mount(path: &str, fs: Box<dyn FileSystem>) {
    let mut guard = get_vfs();
    if let Some(vfs) = &mut *guard {
        vfs.mount(path, fs);
    }
}
