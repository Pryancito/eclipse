//! FAT12/16/32 (vfat) mount support via fatfs 0.4.

use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::cmp::min;
use core::sync::atomic::{AtomicU64, Ordering};

/// Asigna un identificador de dispositivo único a cada FS FAT montado.
/// Empieza alto para no colisionar con los ids de otros sistemas de archivos.
static FAT_DEV_COUNTER: AtomicU64 = AtomicU64::new(0xFA70_0000);

/// Hash FNV-1a de 64 bits del path, usado como número de inodo estable.
/// Evita que dos rutas distintas (o la misma ruta en montajes distintos)
/// compartan identidad (st_dev, st_ino) y confundan a herramientas como `cp`.
fn fnv1a_path(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

use alloc::collections::BTreeMap;
use fatfs::{FileSystem, FsOptions, IoBase, Read, Seek, SeekFrom, Write};
use lock::Mutex;
use rcore_fs::dev::Device;
use rcore_fs::vfs::{
    FileSystem as VfsFileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Timespec,
};

use super::block_mount::{device_from_backend, MountBackend};

fn map_fat_err(e: fatfs::Error<()>) -> FsError {
    match e {
        fatfs::Error::NotFound => FsError::EntryNotFound,
        fatfs::Error::AlreadyExists => FsError::EntryExist,
        fatfs::Error::NotEnoughSpace => FsError::NoDeviceSpace,
        fatfs::Error::DirectoryIsNotEmpty => FsError::DirNotEmpty,
        fatfs::Error::InvalidInput
        | fatfs::Error::InvalidFileNameLength
        | fatfs::Error::UnsupportedFileNameCharacter => FsError::InvalidParam,
        _ => FsError::DeviceError,
    }
}

/// Parent of a FAT path stored without a leading slash. Root (`""`) is its
/// own parent, matching `..` from `/`.
fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => String::new(),
        Some(i) => path[..i].to_string(),
    }
}

struct FatDisk {
    device: Arc<dyn Device>,
    pos: u64,
    len: u64,
}

impl FatDisk {
    fn read_block_bytes(&self, offset: usize, buf: &mut [u8]) -> core::result::Result<usize, ()> {
        if (offset as u64) >= self.len {
            return Ok(0);
        }
        let take = min(buf.len(), (self.len - offset as u64) as usize);
        let n = self
            .device
            .read_at(offset, &mut buf[..take])
            .map_err(|_| ())?;
        // I/O runs under `FatMountFs::inner`'s IRQ-off spinlock. Pump the
        // TLB-shootdown queue so a peer cannot starve for the whole transfer.
        lock::pump();
        Ok(n)
    }

    fn write_block_bytes(&self, offset: usize, buf: &[u8]) -> core::result::Result<usize, ()> {
        if (offset as u64) >= self.len {
            return Ok(0);
        }
        let take = min(buf.len(), (self.len - offset as u64) as usize);
        let n = self.device.write_at(offset, &buf[..take]).map_err(|_| ())?;
        lock::pump();
        Ok(n)
    }
}

impl IoBase for FatDisk {
    type Error = ();
}

impl Read for FatDisk {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, Self::Error> {
        let n = self.read_block_bytes(self.pos as usize, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for FatDisk {
    fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, Self::Error> {
        let n = self.write_block_bytes(self.pos as usize, buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> core::result::Result<(), Self::Error> {
        // Do not call `device.sync()` here: fatfs flushes on every `File` drop
        // (every read/write), and a device flush per op is a large regression.
        // Durability is `FatMountFs::sync` / inode `sync_all`.
        Ok(())
    }
}

impl Seek for FatDisk {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, Self::Error> {
        self.pos = match pos {
            SeekFrom::Start(s) => s,
            SeekFrom::Current(off) => (self.pos as i64 + off) as u64,
            SeekFrom::End(off) => (self.len as i64 + off) as u64,
        };
        Ok(self.pos)
    }
}

#[derive(Clone)]
struct FatDirEntry {
    name: String,
    is_dir: bool,
}

pub struct FatMountFs {
    inner: Mutex<FileSystem<FatDisk>>,
    device: Arc<dyn Device>,
    this: Mutex<Weak<Self>>,
    /// Identificador de dispositivo único de este montaje (para st_dev).
    dev: u64,
    dir_cache: Mutex<BTreeMap<String, Arc<Vec<FatDirEntry>>>>,
}

impl FatMountFs {
    pub fn open(backend: MountBackend) -> rcore_fs::vfs::Result<Arc<Self>> {
        let len = match &backend {
            MountBackend::Block(block) => block.block_count() as u64 * 512,
            MountBackend::File(file) => file.metadata().map(|m| m.size as u64).unwrap_or(0),
        };
        let device = device_from_backend(&backend)?;
        let disk = FatDisk {
            device: device.clone(),
            pos: 0,
            len,
        };
        let fs = FileSystem::new(disk, FsOptions::new()).map_err(|_| FsError::DeviceError)?;
        let arc = Arc::new(Self {
            inner: Mutex::new(fs),
            device,
            this: Mutex::new(Weak::new()),
            dev: FAT_DEV_COUNTER.fetch_add(1, Ordering::Relaxed),
            dir_cache: Mutex::new(BTreeMap::new()),
        });
        *arc.this.lock() = Arc::downgrade(&arc);
        Ok(arc)
    }

    fn arc(&self) -> Arc<Self> {
        self.this.lock().upgrade().expect("FatMountFs dropped")
    }

    fn invalidate_dir(&self, path: &str) {
        self.dir_cache.lock().remove(path);
    }

    fn cached_readdir(&self, path: &str) -> rcore_fs::vfs::Result<Arc<Vec<FatDirEntry>>> {
        if let Some(entries) = self.dir_cache.lock().get(path) {
            return Ok(entries.clone());
        }
        let entries = {
            let fs = self.inner.lock();
            let dir = if path.is_empty() {
                fs.root_dir()
            } else {
                fs.root_dir().open_dir(path).map_err(map_fat_err)?
            };
            let mut cached = Vec::new();
            for entry in dir.iter() {
                let entry = entry.map_err(|_| FsError::DeviceError)?;
                let name = entry.file_name();
                if !name.is_empty() {
                    cached.push(FatDirEntry {
                        name,
                        is_dir: entry.is_dir(),
                    });
                }
            }
            Arc::new(cached)
        };
        self.dir_cache.lock().insert(path.to_string(), entries.clone());
        Ok(entries)
    }
}

impl Drop for FatMountFs {
    fn drop(&mut self) {
        let _ = self.device.sync();
    }
}

impl VfsFileSystem for FatMountFs {
    fn sync(&self) -> rcore_fs::vfs::Result<()> {
        self.device.sync().map_err(|_| FsError::DeviceError)
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(FatMountINode {
            fs: self.arc(),
            path: String::new(),
            is_dir: true,
        })
    }

    fn info(&self) -> FsInfo {
        let fs = self.inner.lock();
        if let Ok(stats) = fs.stats() {
            let cluster_size = stats.cluster_size() as usize;
            FsInfo {
                bsize: cluster_size,
                frsize: cluster_size,
                blocks: stats.total_clusters() as usize,
                bfree: stats.free_clusters() as usize,
                bavail: stats.free_clusters() as usize,
                files: 0,
                ffree: 0,
                namemax: 255,
            }
        } else {
            FsInfo {
                bsize: 512,
                frsize: 512,
                blocks: 0,
                bfree: 0,
                bavail: 0,
                files: 0,
                ffree: 0,
                namemax: 255,
            }
        }
    }
}

struct FatMountINode {
    fs: Arc<FatMountFs>,
    path: String,
    is_dir: bool,
}

impl FatMountINode {
    fn child_path(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_string()
        } else {
            alloc::format!("{}/{}", self.path, name)
        }
    }
}

impl INode for FatMountINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        if self.is_dir {
            return Err(FsError::IsDir);
        }
        let fs = self.fs.inner.lock();
        let mut file = fs
            .root_dir()
            .open_file(&self.path)
            .map_err(|_| FsError::EntryNotFound)?;
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|_| FsError::DeviceError)?;
        file.read(buf).map_err(|_| FsError::DeviceError)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        if self.is_dir {
            return Err(FsError::IsDir);
        }
        let fs = self.fs.inner.lock();
        let mut file = fs
            .root_dir()
            .open_file(&self.path)
            .map_err(|_| FsError::EntryNotFound)?;
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|_| FsError::DeviceError)?;
        file.write(buf).map_err(|_| FsError::DeviceError)
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: !self.is_dir,
            error: false,
        })
    }

    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        let size = if self.is_dir {
            0
        } else {
            let fs = self.fs.inner.lock();
            let mut file = fs
                .root_dir()
                .open_file(&self.path)
                .map_err(|_| FsError::EntryNotFound)?;
            file.seek(SeekFrom::End(0))
                .map_err(|_| FsError::DeviceError)? as usize
        };
        Ok(Metadata {
            dev: self.fs.dev as usize,
            inode: fnv1a_path(&self.path) as usize,
            size,
            blk_size: 512,
            blocks: size.div_ceil(512),
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: if self.is_dir {
                FileType::Dir
            } else {
                FileType::File
            },
            mode: if self.is_dir { 0o755 } else { 0o644 },
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    /// FAT no persiste uid/gid/mode; aceptar la llamada como no-op.
    ///
    /// `open(O_CREAT)` invoca `set_metadata` justo después de crear el fichero
    /// (initialize_created_metadata). Con el default del trait (NotSupported →
    /// ENOSYS) cualquier creación de fichero sobre vfat fallaba con
    /// "Function not implemented".
    fn set_metadata(&self, _metadata: &Metadata) -> rcore_fs::vfs::Result<()> {
        Ok(())
    }

    fn find(&self, name: &str) -> rcore_fs::vfs::Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(FatMountINode {
                fs: self.fs.clone(),
                path: self.path.clone(),
                is_dir: self.is_dir,
            })),
            ".." => Ok(Arc::new(FatMountINode {
                fs: self.fs.clone(),
                path: parent_path(&self.path),
                is_dir: true,
            })),
            name => {
                let entries = self.fs.cached_readdir(&self.path)?;
                for entry in entries.iter() {
                    // FAT es case-insensitive: una entrada 8.3 puede estar
                    // almacenada como "BOOTX64.EFI" y buscarse "BootX64.efi".
                    // Con comparación exacta el lookup fallaba (EntryNotFound)
                    // y open(O_CREAT) intentaba re-crear el fichero existente.
                    if entry.name.eq_ignore_ascii_case(name) {
                        return Ok(Arc::new(FatMountINode {
                            fs: self.fs.clone(),
                            // Usar el nombre tal como está en el directorio para
                            // que open_file/open_dir posteriores lo encuentren.
                            path: self.child_path(&entry.name),
                            is_dir: entry.is_dir,
                        }));
                    }
                }
                Err(FsError::EntryNotFound)
            }
        }
    }

    fn get_entry(&self, id: usize) -> rcore_fs::vfs::Result<String> {
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => {
                let entries = self.fs.cached_readdir(&self.path)?;
                entries
                    .get(i - 2)
                    .map(|e| e.name.clone())
                    .ok_or(FsError::EntryNotFound)
            }
        }
    }

    fn create(
        &self,
        name: &str,
        type_: rcore_fs::vfs::FileType,
        _mode: u32,
    ) -> rcore_fs::vfs::Result<Arc<dyn INode>> {
        if !self.is_dir {
            return Err(FsError::NotDir);
        }
        {
            let fs = self.fs.inner.lock();
            let dir = if self.path.is_empty() {
                fs.root_dir()
            } else {
                fs.root_dir()
                    .open_dir(&self.path)
                    .map_err(|_| FsError::EntryNotFound)?
            };
            match type_ {
                rcore_fs::vfs::FileType::File => {
                    dir.create_file(name).map_err(map_fat_err)?;
                }
                rcore_fs::vfs::FileType::Dir => {
                    dir.create_dir(name).map_err(map_fat_err)?;
                }
                _ => return Err(FsError::NotSupported),
            }
        }
        self.fs.invalidate_dir(&self.path);
        Ok(Arc::new(FatMountINode {
            fs: self.fs.clone(),
            path: self.child_path(name),
            is_dir: type_ == rcore_fs::vfs::FileType::Dir,
        }))
    }

    fn unlink(&self, name: &str) -> rcore_fs::vfs::Result<()> {
        if !self.is_dir {
            return Err(FsError::NotDir);
        }
        {
            let fs = self.fs.inner.lock();
            let dir = if self.path.is_empty() {
                fs.root_dir()
            } else {
                fs.root_dir()
                    .open_dir(&self.path)
                    .map_err(|_| FsError::EntryNotFound)?
            };
            dir.remove(name).map_err(map_fat_err)?;
        }
        self.fs.invalidate_dir(&self.path);
        Ok(())
    }

    fn resize(&self, len: usize) -> rcore_fs::vfs::Result<()> {
        if self.is_dir {
            return Err(FsError::IsDir);
        }
        let fs = self.fs.inner.lock();
        let mut file = fs
            .root_dir()
            .open_file(&self.path)
            .map_err(|_| FsError::EntryNotFound)?;
        let cur = file
            .seek(SeekFrom::End(0))
            .map_err(|_| FsError::DeviceError)? as usize;
        if len < cur {
            file.seek(SeekFrom::Start(len as u64))
                .map_err(|_| FsError::DeviceError)?;
            file.truncate().map_err(|_| FsError::DeviceError)?;
        } else if len > cur {
            // fatfs clamps seek-past-EOF, so a single write at `len-1` only
            // grows the file by one byte. Extend by writing zeros from EOF.
            let zeros = [0u8; 512];
            let mut pos = cur;
            while pos < len {
                let n = (len - pos).min(zeros.len());
                let w = file.write(&zeros[..n]).map_err(map_fat_err)?;
                if w == 0 {
                    return Err(FsError::NoDeviceSpace);
                }
                pos += w;
            }
        }
        Ok(())
    }

    fn sync_all(&self) -> rcore_fs::vfs::Result<()> {
        self.fs.sync()
    }

    fn sync_data(&self) -> rcore_fs::vfs::Result<()> {
        self.fs.sync()
    }

    fn fs(&self) -> Arc<dyn VfsFileSystem> {
        self.fs.clone()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

pub fn open_fat(backend: MountBackend) -> rcore_fs::vfs::Result<Arc<dyn VfsFileSystem>> {
    FatMountFs::open(backend).map(|fs| fs as Arc<dyn VfsFileSystem>)
}
