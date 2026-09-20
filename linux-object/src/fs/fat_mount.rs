//! FAT12/16/32 (vfat) mount support via fatfs 0.4.

use alloc::collections::BTreeMap;
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

use fatfs::{Date, DateTime, FileSystem, FsOptions, IoBase, Read, Seek, SeekFrom, Time, Write};
use lock::Mutex;
use rcore_fs::dev::Device;
use rcore_fs::vfs::{
    FileSystem as VfsFileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Timespec,
};

use super::block_mount::{backend_size, device_from_backend, MountBackend};

/// Tamaño máximo que FAT puede representar en una entrada de directorio.
const FAT_MAX_FILE_SIZE: u64 = u32::MAX as u64;

/// Trozo de ceros usado para extender un fichero al escribir más allá del
/// final. Un búfer en la pila evita reservar memoria en el camino de escritura.
const ZERO_CHUNK: usize = 4096;

// ---------------------------------------------------------------------------
// Reloj
// ---------------------------------------------------------------------------

/// Días desde 1970-01-01 para una fecha del calendario gregoriano.
/// Algoritmo `days_from_civil` de Howard Hinnant.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inversa de [`days_from_civil`]: `(año, mes, día)` a partir de los días
/// transcurridos desde 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = (mp + 2) % 12 + 1;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Límites del calendario DOS. `Date::new` entra en pánico fuera de este rango,
/// así que todo lo que venga del reloj se recorta antes de construir una fecha.
const DOS_MIN_YEAR: i64 = 1980;
const DOS_MAX_YEAR: i64 = 2107;

/// Convierte segundos unix a una fecha y hora DOS, recortando al rango que FAT
/// puede representar. Nunca entra en pánico: un reloj sin inicializar (0, o sea
/// 1970) daría un año fuera de rango y tumbaría el kernel.
fn dos_from_unix(secs: i64) -> DateTime {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (mut y, mut mo, mut d) = civil_from_days(days);
    let (mut h, mut mi, mut s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    if y < DOS_MIN_YEAR {
        y = DOS_MIN_YEAR;
        mo = 1;
        d = 1;
        h = 0;
        mi = 0;
        s = 0;
    } else if y > DOS_MAX_YEAR {
        y = DOS_MAX_YEAR;
        mo = 12;
        d = 31;
        h = 23;
        mi = 59;
        s = 59;
    }
    DateTime::new(
        Date::new(y as u16, mo as u16, d as u16),
        // FAT guarda los segundos con resolución de 2 s; el campo de
        // milisegundos cubre el segundo impar. `Time::new` exige 0..=59.
        Time::new(h as u16, mi as u16, s as u16, 0),
    )
}

/// Convierte una fecha DOS a segundos unix.
fn unix_from_dos_date(date: Date) -> i64 {
    days_from_civil(date.year as i64, date.month as i64, date.day as i64) * 86_400
}

/// Convierte una fecha y hora DOS a segundos unix.
fn unix_from_dos(dt: DateTime) -> i64 {
    unix_from_dos_date(dt.date)
        + dt.time.hour as i64 * 3600
        + dt.time.min as i64 * 60
        + dt.time.sec as i64
}

/// `TimeProvider` de fatfs respaldado por el reloj de pared del kernel.
///
/// Sin esto fatfs usa `NullTimeProvider` en `no_std`, que sella cada fichero
/// nuevo con 1980-01-01: `make`, `tar` y cualquier cosa que compare mtimes se
/// comporta mal sobre un montaje FAT.
#[derive(Debug, Clone, Copy)]
struct KernelTimeProvider;

impl KernelTimeProvider {
    fn now(&self) -> DateTime {
        let now = kernel_hal::timer::wall_clock_now();
        dos_from_unix(now.as_secs() as i64)
    }
}

impl fatfs::TimeProvider for KernelTimeProvider {
    fn get_current_date(&self) -> Date {
        self.now().date
    }
    fn get_current_date_time(&self) -> DateTime {
        self.now()
    }
}

type FatFs = FileSystem<FatDisk, KernelTimeProvider>;

// ---------------------------------------------------------------------------
// Dispositivo
// ---------------------------------------------------------------------------

/// Vista de flujo (`Read`/`Write`/`Seek`) sobre el dispositivo del montaje.
///
/// Va por `device_from_backend`, o sea por `CachedDevice`, igual que btrfs y
/// ext2. Antes FAT hablaba con `BlockScheme` en crudo y se saltaba la caché de
/// bloques entera: como `fatfs` recorre la cadena de clústeres desde el
/// principio en cada `seek`, y `read_at`/`write_at` reabren el fichero por ruta
/// en cada llamada, leer en el desplazamiento N costaba O(N) comandos reales al
/// disco.
struct FatDisk {
    dev: Arc<dyn Device>,
    len: u64,
    pos: u64,
}

impl FatDisk {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> core::result::Result<usize, ()> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let take = min(buf.len() as u64, self.len - offset) as usize;
        let n = self
            .dev
            .read_at(offset as usize, &mut buf[..take])
            .map_err(|_| ())?;
        // Esta E/S corre bajo el spinlock con IRQs desactivadas de
        // `FatMountFs::inner`. Bombear la cola de shootdown de TLB entre
        // transferencias evita que un shootdown ajeno se quede esperando todo
        // un recorrido de directorio: era la firma del pánico en hardware real.
        lock::pump();
        Ok(n)
    }

    fn write_bytes(&self, offset: u64, buf: &[u8]) -> core::result::Result<usize, ()> {
        if buf.is_empty() {
            return Ok(0);
        }
        if offset >= self.len {
            // Más allá del final del dispositivo. Devolver `Ok(0)` aquí sería
            // "no escribí nada" y puede colgar a quien reintente en bucle,
            // además de perder datos en silencio; es un error de verdad.
            return Err(());
        }
        let take = min(buf.len() as u64, self.len - offset) as usize;
        let n = self
            .dev
            .write_at(offset as usize, &buf[..take])
            .map_err(|_| ())?;
        lock::pump();
        if n == 0 {
            return Err(());
        }
        Ok(n)
    }
}

impl IoBase for FatDisk {
    type Error = ();
}

impl Read for FatDisk {
    fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, Self::Error> {
        let n = self.read_bytes(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for FatDisk {
    fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, Self::Error> {
        let n = self.write_bytes(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> core::result::Result<(), Self::Error> {
        // `CachedDevice` es write-through: lo ya reconocido está en el disco.
        // El vaciado explícito del dispositivo lo hace `FatMountFs::sync`.
        Ok(())
    }
}

impl Seek for FatDisk {
    fn seek(&mut self, pos: SeekFrom) -> core::result::Result<u64, Self::Error> {
        let new: i64 = match pos {
            SeekFrom::Start(s) => (s <= i64::MAX as u64).then_some(s as i64),
            // Antes esto hacía `as u64` sobre la suma: un desplazamiento
            // negativo envolvía a un offset enorme en vez de fallar.
            SeekFrom::Current(off) => (self.pos as i64).checked_add(off),
            SeekFrom::End(off) => (self.len as i64).checked_add(off),
        }
        .filter(|v| *v >= 0)
        .ok_or(())?;
        self.pos = new as u64;
        Ok(self.pos)
    }
}

// ---------------------------------------------------------------------------
// Sistema de archivos
// ---------------------------------------------------------------------------

/// Una entrada de directorio ya leída. Lo que devuelve `readdir` y `stat` sin
/// volver a recorrer el directorio.
#[derive(Clone)]
struct CachedEntry {
    name: String,
    is_dir: bool,
    size: u64,
    atime: i64,
    mtime: i64,
    ctime: i64,
}

pub struct FatMountFs {
    /// `None` sólo mientras `sync` cambia una instancia por otra, y de forma
    /// permanente si ese relevo falla (el montaje queda inservible a propósito
    /// en vez de seguir con un estado a medias).
    inner: Mutex<Option<FatFs>>,
    this: Mutex<Weak<Self>>,
    /// Identificador de dispositivo único de este montaje (para st_dev).
    dev_id: u64,
    /// El mismo dispositivo que usa `inner`, para poder hacer `sync`.
    dev: Arc<dyn Device>,
    /// Tamaño del dispositivo, para reconstruir el `FileSystem` en `sync`.
    len: u64,
    /// Tamaño de clúster, para `blk_size` y `st_blocks`.
    cluster_size: u32,
    /// Listados de directorio cacheados, indexados por el hash de la ruta. Se
    /// invalida el directorio entero en cualquier mutación que lo afecte.
    ///
    /// Sin esto un `readdir` completo era O(n²): `get_entry(id)` reconstruía la
    /// lista de nombres entera en cada llamada para devolver el elemento `id`.
    dir_cache: Mutex<BTreeMap<u64, Arc<Vec<CachedEntry>>>>,
}

impl FatMountFs {
    pub fn open(backend: &MountBackend) -> rcore_fs::vfs::Result<Arc<Self>> {
        let len = backend_size(backend)?;
        let dev = device_from_backend(backend)?;
        let disk = FatDisk {
            dev: dev.clone(),
            len,
            pos: 0,
        };
        let fs = FileSystem::new(disk, FsOptions::new().time_provider(KernelTimeProvider))
            .map_err(|_| FsError::DeviceError)?;
        let cluster_size = fs.cluster_size();
        let arc = Arc::new(Self {
            inner: Mutex::new(Some(fs)),
            this: Mutex::new(Weak::new()),
            dev_id: FAT_DEV_COUNTER.fetch_add(1, Ordering::Relaxed),
            dev,
            len,
            cluster_size,
            dir_cache: Mutex::new(BTreeMap::new()),
        });
        *arc.this.lock() = Arc::downgrade(&arc);
        Ok(arc)
    }

    fn arc(&self) -> Arc<Self> {
        self.this.lock().upgrade().expect("FatMountFs dropped")
    }

    fn invalidate_dir(&self, path: &str) {
        self.dir_cache.lock().remove(&fnv1a_path(path));
    }

    /// Listado de `path`, desde la caché o leyéndolo del disco.
    fn listing(&self, fs: &FatFs, path: &str) -> rcore_fs::vfs::Result<Arc<Vec<CachedEntry>>> {
        let key = fnv1a_path(path);
        if let Some(cached) = self.dir_cache.lock().get(&key) {
            return Ok(cached.clone());
        }
        let dir = open_dir(fs, path)?;
        let mut entries = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(|_| FsError::DeviceError)?;
            let name = entry.file_name();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            entries.push(CachedEntry {
                name,
                is_dir: entry.is_dir(),
                size: entry.len(),
                atime: unix_from_dos_date(entry.accessed()),
                mtime: unix_from_dos(entry.modified()),
                ctime: unix_from_dos(entry.created()),
            });
        }
        let entries = Arc::new(entries);
        self.dir_cache.lock().insert(key, entries.clone());
        Ok(entries)
    }
}

/// Abre un directorio por ruta; la ruta vacía es la raíz.
fn open_dir<'a>(
    fs: &'a FatFs,
    path: &str,
) -> rcore_fs::vfs::Result<fatfs::Dir<'a, FatDisk, KernelTimeProvider, fatfs::LossyOemCpConverter>>
{
    if path.is_empty() {
        Ok(fs.root_dir())
    } else {
        fs.root_dir()
            .open_dir(path)
            .map_err(|_| FsError::EntryNotFound)
    }
}

/// Divide una ruta en (directorio padre, nombre).
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

impl VfsFileSystem for FatMountFs {
    /// Deja el volumen en disco como si se acabase de desmontar limpiamente,
    /// y vacía el dispositivo.
    ///
    /// Antes esto era `Ok(())`: `sync()`/`fsync()` decían que sí sin que nada
    /// llegase al plato. Vaciar el dispositivo no basta: `fatfs` marca el
    /// volumen como sucio en la primera escritura y sólo limpia ese bit (y
    /// escribe el sector FSInfo con el recuento de clústeres libres) al
    /// desmontar. Un kernel no desmonta casi nunca, así que cualquier volumen
    /// FAT que tocase Eclipse acababa en manos de Linux o Windows marcado como
    /// "no desmontado correctamente", y con un FSInfo obsoleto. `fsck.vfat` lo
    /// confirma sobre una imagen que hayamos escrito.
    ///
    /// `unmount()` consume el `FileSystem`, así que lo sacamos, lo desmontamos
    /// y montamos otro encima del mismo dispositivo. La instancia nueva lee el
    /// bit ya limpio, de modo que la siguiente escritura lo vuelve a marcar
    /// sucio: el protocolo de FAT sigue funcionando.
    fn sync(&self) -> rcore_fs::vfs::Result<()> {
        let mut guard = self.inner.lock();
        let unmounted = match guard.take() {
            Some(fs) => fs.unmount().map_err(|_| FsError::DeviceError),
            None => Err(FsError::DeviceError),
        };
        let disk = FatDisk {
            dev: self.dev.clone(),
            len: self.len,
            pos: 0,
        };
        match FileSystem::new(disk, FsOptions::new().time_provider(KernelTimeProvider)) {
            Ok(fs) => *guard = Some(fs),
            Err(_) => {
                // Sin instancia el montaje queda inservible; mejor eso que
                // seguir con una que no refleje el disco.
                warn!("fat: remount after sync failed; the mount is now unusable");
                return Err(FsError::DeviceError);
            }
        }
        drop(guard);
        unmounted?;
        self.dev.sync().map_err(|_| FsError::DeviceError)
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(FatMountINode {
            fs: self.arc(),
            path: String::new(),
            is_dir: true,
        })
    }

    fn info(&self) -> FsInfo {
        let guard = self.inner.lock();
        let unknown = FsInfo {
            bsize: self.cluster_size as usize,
            frsize: self.cluster_size as usize,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
        };
        let fs = match guard.as_ref() {
            Some(fs) => fs,
            None => return unknown,
        };
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
                bsize: self.cluster_size as usize,
                frsize: self.cluster_size as usize,
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

    /// Entrada de directorio de este inodo, tal como está en su padre.
    /// La raíz no tiene entrada propia.
    fn own_entry(&self, fs: &FatFs) -> rcore_fs::vfs::Result<Option<CachedEntry>> {
        if self.path.is_empty() {
            return Ok(None);
        }
        let (parent, name) = split_path(&self.path);
        let entries = self.fs.listing(fs, parent)?;
        entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .cloned()
            .map(Some)
            .ok_or(FsError::EntryNotFound)
    }

    /// El directorio cuyo listado deja de ser válido cuando cambia este inodo.
    fn parent_dir(&self) -> &str {
        split_path(&self.path).0
    }

    /// Extiende el fichero con ceros hasta `target`.
    ///
    /// `fatfs::File::seek` **recorta** el desplazamiento al tamaño actual
    /// (file.rs: *"Seek beyond the end of the file"*), así que no se puede
    /// saltar más allá del final: hay que escribir los ceros. De no hacerlo, un
    /// `pwrite()` más allá del final aterrizaba en el sitio equivocado y
    /// `ftruncate()` para agrandar dejaba el fichero 1 byte más grande, dijeras
    /// lo que dijeras.
    fn extend_to(
        file: &mut fatfs::File<'_, FatDisk, KernelTimeProvider, fatfs::LossyOemCpConverter>,
        from: u64,
        target: u64,
    ) -> rcore_fs::vfs::Result<()> {
        let zeros = [0u8; ZERO_CHUNK];
        file.seek(SeekFrom::Start(from))
            .map_err(|_| FsError::DeviceError)?;
        let mut at = from;
        while at < target {
            let take = min((target - at) as usize, ZERO_CHUNK);
            let n = file
                .write(&zeros[..take])
                .map_err(|_| FsError::NoDeviceSpace)?;
            if n == 0 {
                return Err(FsError::NoDeviceSpace);
            }
            at += n as u64;
        }
        Ok(())
    }
}

impl INode for FatMountINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        if self.is_dir {
            return Err(FsError::IsDir);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let mut file = fs
            .root_dir()
            .open_file(&self.path)
            .map_err(|_| FsError::EntryNotFound)?;
        let at = file
            .seek(SeekFrom::Start(offset as u64))
            .map_err(|_| FsError::DeviceError)?;
        if at != offset as u64 {
            // El seek se recortó: el desplazamiento está más allá del final.
            return Ok(0);
        }
        // `fatfs::File::read` para en el límite del clúster. Hacer un solo
        // `read` devolvía lecturas cortas gratuitas y un viaje extra por
        // clúster a la capa de arriba.
        let mut done = 0;
        while done < buf.len() {
            let n = file
                .read(&mut buf[done..])
                .map_err(|_| FsError::DeviceError)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        if self.is_dir {
            return Err(FsError::IsDir);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let offset = offset as u64;
        if offset.saturating_add(buf.len() as u64) > FAT_MAX_FILE_SIZE {
            return Err(FsError::InvalidParam);
        }
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let mut file = fs
            .root_dir()
            .open_file(&self.path)
            .map_err(|_| FsError::EntryNotFound)?;
        let size = file
            .seek(SeekFrom::End(0))
            .map_err(|_| FsError::DeviceError)?;
        if offset > size {
            // Rellenar el hueco: FAT no tiene ficheros dispersos y el seek no
            // pasa del final.
            Self::extend_to(&mut file, size, offset)?;
        } else {
            file.seek(SeekFrom::Start(offset))
                .map_err(|_| FsError::DeviceError)?;
        }
        let mut done = 0;
        while done < buf.len() {
            let n = file
                .write(&buf[done..])
                .map_err(|_| FsError::NoDeviceSpace)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        // `Drop` vacía la entrada de directorio pero se traga el error; si el
        // tamaño nuevo no llega al disco, el fichero queda truncado.
        file.flush().map_err(|_| FsError::DeviceError)?;
        drop(file);
        drop(guard);
        self.fs.invalidate_dir(self.parent_dir());
        if done == 0 {
            return Err(FsError::NoDeviceSpace);
        }
        Ok(done)
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: !self.is_dir,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let entry = self.own_entry(fs)?;
        drop(guard);
        let (size, atime, mtime, ctime) = match &entry {
            Some(e) => (e.size, e.atime, e.mtime, e.ctime),
            None => (0, 0, 0, 0),
        };
        let size = if self.is_dir { 0 } else { size as usize };
        let blk_size = self.fs.cluster_size as usize;
        Ok(Metadata {
            dev: self.fs.dev_id as usize,
            inode: fnv1a_path(&self.path) as usize,
            size,
            blk_size,
            // `st_blocks` se cuenta siempre en unidades de 512 bytes.
            blocks: size.div_ceil(512),
            atime: Timespec {
                sec: atime,
                nsec: 0,
            },
            mtime: Timespec {
                sec: mtime,
                nsec: 0,
            },
            ctime: Timespec {
                sec: ctime,
                nsec: 0,
            },
            type_: if self.is_dir {
                FileType::Dir
            } else {
                FileType::File
            },
            mode: if self.is_dir { 0o755 } else { 0o644 },
            // Un directorio tiene al menos "." y la entrada de su padre.
            nlinks: if self.is_dir { 2 } else { 1 },
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
        if !self.is_dir {
            return Err(FsError::NotDir);
        }
        match name {
            "." => Ok(Arc::new(FatMountINode {
                fs: self.fs.clone(),
                path: self.path.clone(),
                is_dir: self.is_dir,
            })),
            // El padre es un prefijo de nuestra propia ruta. Antes esto
            // devolvía EntryNotFound, así que `cd ..` fallaba dentro de un
            // montaje FAT aunque `readdir` sí listaba "..".
            ".." => Ok(Arc::new(FatMountINode {
                fs: self.fs.clone(),
                path: split_path(&self.path).0.to_string(),
                is_dir: true,
            })),
            name => {
                let guard = self.fs.inner.lock();
                let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
                let entries = self.fs.listing(fs, &self.path)?;
                drop(guard);
                // FAT es case-insensitive: una entrada 8.3 puede estar
                // almacenada como "BOOTX64.EFI" y buscarse "BootX64.efi".
                // Con comparación exacta el lookup fallaba (EntryNotFound)
                // y open(O_CREAT) intentaba re-crear el fichero existente.
                let entry = entries
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(name))
                    .ok_or(FsError::EntryNotFound)?;
                Ok(Arc::new(FatMountINode {
                    fs: self.fs.clone(),
                    // Usar el nombre tal como está en el directorio para
                    // que open_file/open_dir posteriores lo encuentren.
                    path: self.child_path(&entry.name),
                    is_dir: entry.is_dir,
                }))
            }
        }
    }

    fn get_entry(&self, id: usize) -> rcore_fs::vfs::Result<String> {
        if !self.is_dir {
            return Err(FsError::NotDir);
        }
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => {
                let guard = self.fs.inner.lock();
                let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
                let entries = self.fs.listing(fs, &self.path)?;
                drop(guard);
                entries
                    .get(i - 2)
                    .map(|e| e.name.clone())
                    .ok_or(FsError::EntryNotFound)
            }
        }
    }

    fn get_entry_with_metadata(&self, id: usize) -> rcore_fs::vfs::Result<(Metadata, String)> {
        if !self.is_dir {
            return Err(FsError::NotDir);
        }
        if id < 2 {
            let name = self.get_entry(id)?;
            return Ok((self.metadata()?, name));
        }
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let entries = self.fs.listing(fs, &self.path)?;
        drop(guard);
        let entry = entries.get(id - 2).ok_or(FsError::EntryNotFound)?;
        let child = FatMountINode {
            fs: self.fs.clone(),
            path: self.child_path(&entry.name),
            is_dir: entry.is_dir,
        };
        Ok((child.metadata()?, entry.name.clone()))
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
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let dir = open_dir(fs, &self.path)?;
        match type_ {
            rcore_fs::vfs::FileType::File => {
                let _ = dir.create_file(name).map_err(|_| FsError::NoDeviceSpace)?;
            }
            rcore_fs::vfs::FileType::Dir => {
                let _ = dir.create_dir(name).map_err(|_| FsError::NoDeviceSpace)?;
            }
            _ => return Err(FsError::NotSupported),
        }
        drop(dir);
        drop(guard);
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
        if name == "." || name == ".." {
            return Err(FsError::DirNotEmpty);
        }
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let dir = open_dir(fs, &self.path)?;
        let res = dir.remove(name).map_err(|e| match e {
            fatfs::Error::DirectoryIsNotEmpty => FsError::DirNotEmpty,
            fatfs::Error::NotFound => FsError::EntryNotFound,
            _ => FsError::DeviceError,
        });
        drop(dir);
        drop(guard);
        if res.is_ok() {
            self.fs.invalidate_dir(&self.path);
            self.fs.invalidate_dir(&self.child_path(name));
        }
        res
    }

    /// Renombrar/mover dentro del mismo montaje.
    ///
    /// Antes no se implementaba, así que caía en el default del trait y `mv`
    /// sobre FAT devolvía ENOSYS.
    fn move_(
        &self,
        old_name: &str,
        target: &Arc<dyn INode>,
        new_name: &str,
    ) -> rcore_fs::vfs::Result<()> {
        if !self.is_dir {
            return Err(FsError::NotDir);
        }
        let dest = target
            .as_any_ref()
            .downcast_ref::<FatMountINode>()
            .ok_or(FsError::NotSupported)?;
        if !dest.is_dir {
            return Err(FsError::NotDir);
        }
        // Mover entre dos montajes FAT distintos no lo puede hacer `fatfs`.
        if !Arc::ptr_eq(&self.fs, &dest.fs) {
            return Err(FsError::NotSupported);
        }
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return Err(FsError::InvalidParam);
        }
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let src_dir = open_dir(fs, &self.path)?;
        let dst_dir = open_dir(fs, &dest.path)?;
        let res = src_dir
            .rename(old_name, &dst_dir, new_name)
            .map_err(|e| match e {
                fatfs::Error::NotFound => FsError::EntryNotFound,
                fatfs::Error::AlreadyExists => FsError::EntryExist,
                fatfs::Error::DirectoryIsNotEmpty => FsError::DirNotEmpty,
                _ => FsError::DeviceError,
            });
        drop(dst_dir);
        drop(src_dir);
        drop(guard);
        if res.is_ok() {
            self.fs.invalidate_dir(&self.path);
            self.fs.invalidate_dir(&dest.path);
            self.fs.invalidate_dir(&self.child_path(old_name));
        }
        res
    }

    fn resize(&self, len: usize) -> rcore_fs::vfs::Result<()> {
        if self.is_dir {
            return Err(FsError::IsDir);
        }
        let len = len as u64;
        if len > FAT_MAX_FILE_SIZE {
            return Err(FsError::InvalidParam);
        }
        let guard = self.fs.inner.lock();
        let fs = guard.as_ref().ok_or(FsError::DeviceError)?;
        let mut file = fs
            .root_dir()
            .open_file(&self.path)
            .map_err(|_| FsError::EntryNotFound)?;
        let cur = file
            .seek(SeekFrom::End(0))
            .map_err(|_| FsError::DeviceError)?;
        if len > cur {
            // Antes esto hacía seek(len-1) + escribir 1 byte; como el seek se
            // recorta al tamaño actual, el fichero crecía exactamente 1 byte
            // pidieras lo que pidieras.
            Self::extend_to(&mut file, cur, len)?;
        } else if len < cur {
            file.seek(SeekFrom::Start(len))
                .map_err(|_| FsError::DeviceError)?;
            file.truncate().map_err(|_| FsError::DeviceError)?;
        }
        file.flush().map_err(|_| FsError::DeviceError)?;
        drop(file);
        drop(guard);
        self.fs.invalidate_dir(self.parent_dir());
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

pub fn open_fat(backend: &MountBackend) -> rcore_fs::vfs::Result<Arc<dyn VfsFileSystem>> {
    FatMountFs::open(backend).map(|fs| fs as Arc<dyn VfsFileSystem>)
}

#[cfg(test)]
mod fat_tests {
    //! Host tests for the vfat adapter. They run the real `fatfs` driver over an
    //! in-memory block device through the same `CachedDevice` path the kernel
    //! uses, so a regression in the VFS glue (offsets, truncation, lookup,
    //! rename, timestamps) is caught here instead of on hardware.
    use super::*;
    use alloc::vec;
    use core::sync::atomic::AtomicUsize;
    use zcore_drivers::scheme::{BlockScheme, Scheme};
    use zcore_drivers::{DeviceError, DeviceResult};

    // `linux-object` is `no_std`, but the host test runner links std anyway:
    // pulling it in explicitly lets the cross-validation test below shell out to
    // dosfstools/mtools.
    extern crate std;

    struct MockBlock {
        sectors: Mutex<Vec<u8>>,
        nsec: usize,
        flushes: AtomicUsize,
    }

    impl MockBlock {
        fn new(nsec: usize) -> Arc<Self> {
            Arc::new(Self {
                sectors: Mutex::new(vec![0u8; nsec * 512]),
                nsec,
                flushes: AtomicUsize::new(0),
            })
        }
        fn flushes(&self) -> usize {
            self.flushes.load(Ordering::Relaxed)
        }
    }

    impl Scheme for MockBlock {
        fn name(&self) -> &str {
            "mockblock"
        }
    }

    impl BlockScheme for MockBlock {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
            if buf.is_empty() || buf.len() % 512 != 0 {
                return Err(DeviceError::InvalidParam);
            }
            let start = block_id * 512;
            let d = self.sectors.lock();
            if start + buf.len() > d.len() {
                return Err(DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&d[start..start + buf.len()]);
            Ok(())
        }
        fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
            if buf.is_empty() || buf.len() % 512 != 0 {
                return Err(DeviceError::InvalidParam);
            }
            let start = block_id * 512;
            let mut d = self.sectors.lock();
            if start + buf.len() > d.len() {
                return Err(DeviceError::InvalidParam);
            }
            d[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.nsec
        }
    }

    /// A freshly formatted 64 MiB vfat volume, mounted the way the kernel does.
    fn mount(nsec: usize) -> (Arc<MockBlock>, Arc<FatMountFs>) {
        let block = MockBlock::new(nsec);
        let backend = MountBackend::Block(block.clone());
        let dev = device_from_backend(&backend).unwrap();
        let mut disk = FatDisk {
            dev,
            len: nsec as u64 * 512,
            pos: 0,
        };
        fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).unwrap();
        let fs = FatMountFs::open(&backend).unwrap();
        (block, fs)
    }

    fn mount_default() -> (Arc<MockBlock>, Arc<FatMountFs>) {
        mount(128 * 1024) // 64 MiB
    }

    fn create_file(root: &Arc<dyn INode>, name: &str) -> Arc<dyn INode> {
        root.create(name, FileType::File, 0o644).unwrap()
    }

    /// `resize()` grew the file by exactly one byte whatever you asked for:
    /// it seeked to `len - 1` and wrote a byte, but `fatfs::File::seek` clamps
    /// past-the-end offsets to the current size, so the seek landed at 0.
    #[test]
    fn resize_grows_to_the_requested_length() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let file = create_file(&root, "grow.bin");

        file.resize(100_000).unwrap();
        assert_eq!(file.metadata().unwrap().size, 100_000);

        // The whole of it must read back as zeros, not just be a size claim.
        let mut buf = vec![0xAAu8; 100_000];
        let n = file.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 100_000, "short read over the grown region");
        assert!(buf.iter().all(|&b| b == 0), "grown region is not zeroed");

        // Shrinking still works, and re-growing after a shrink too.
        file.resize(1_000).unwrap();
        assert_eq!(file.metadata().unwrap().size, 1_000);
        file.resize(5_000).unwrap();
        assert_eq!(file.metadata().unwrap().size, 5_000);
    }

    /// Same clamp, worse consequence: a `pwrite()` past the end used to land at
    /// the current end of file instead of the offset asked for, silently.
    #[test]
    fn write_past_end_lands_at_the_requested_offset() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let file = create_file(&root, "sparse.bin");

        assert_eq!(file.write_at(0, b"hello").unwrap(), 5);
        assert_eq!(file.write_at(10_000, b"world").unwrap(), 5);
        assert_eq!(file.metadata().unwrap().size, 10_005);

        let mut buf = vec![0xAAu8; 10_005];
        let mut done = 0;
        while done < buf.len() {
            let n = file.read_at(done, &mut buf[done..]).unwrap();
            assert!(n > 0, "read stalled at {}", done);
            done += n;
        }
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(&buf[10_000..], b"world");
        assert!(
            buf[5..10_000].iter().all(|&b| b == 0),
            "the gap was not zero-filled",
        );
    }

    /// `find("..")` returned EntryNotFound, so `cd ..` failed inside a FAT
    /// mount even though `readdir` listed `..`.
    #[test]
    fn dotdot_resolves_to_the_parent() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let a = root.create("a", FileType::Dir, 0o755).unwrap();
        let b = a.create("b", FileType::Dir, 0o755).unwrap();
        create_file(&b, "leaf.txt");

        let up = b.find("..").unwrap();
        assert!(up.find("b").is_ok(), "`..` from a/b is not a");

        let up2 = up.find("..").unwrap();
        assert!(up2.find("a").is_ok(), "`..` from a is not the root");

        // `..` at the root stays at the root rather than escaping the mount.
        let up3 = up2.find("..").unwrap();
        assert!(up3.find("a").is_ok());

        assert!(b.find(".").unwrap().find("leaf.txt").is_ok());
    }

    /// `move_` was not implemented at all, so `mv` on FAT returned ENOSYS.
    #[test]
    fn rename_within_and_across_directories() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let src = root.create("src", FileType::Dir, 0o755).unwrap();
        let dst = root.create("dst", FileType::Dir, 0o755).unwrap();
        let file = create_file(&src, "a.txt");
        file.write_at(0, b"payload").unwrap();

        // Rename in place.
        src.move_("a.txt", &src, "b.txt").unwrap();
        assert_eq!(src.find("a.txt").err(), Some(FsError::EntryNotFound));
        let moved = src.find("b.txt").unwrap();
        let mut buf = [0u8; 7];
        assert_eq!(moved.read_at(0, &mut buf).unwrap(), 7);
        assert_eq!(&buf, b"payload");

        // Move to another directory.
        src.move_("b.txt", &dst, "c.txt").unwrap();
        assert_eq!(src.find("b.txt").err(), Some(FsError::EntryNotFound));
        let moved = dst.find("c.txt").unwrap();
        let mut buf = [0u8; 7];
        assert_eq!(moved.read_at(0, &mut buf).unwrap(), 7);
        assert_eq!(&buf, b"payload");

        // A rename into a different FAT mount is refused rather than silently
        // doing the wrong thing.
        let (_blk2, other) = mount_default();
        let other_root = other.root_inode();
        assert_eq!(
            dst.move_("c.txt", &other_root, "c.txt").unwrap_err(),
            FsError::NotSupported,
        );
    }

    /// Timestamps used to be hardcoded to zero, and new files were stamped
    /// 1980-01-01 because no `TimeProvider` was installed.
    #[test]
    fn timestamps_come_from_the_directory_entry() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let file = create_file(&root, "stamped.txt");
        file.write_at(0, b"x").unwrap();

        let md = file.metadata().unwrap();
        // 1980-01-01, the floor of the DOS calendar, as unix seconds.
        let dos_epoch = 315_532_800;
        assert!(
            md.mtime.sec >= dos_epoch,
            "mtime {} is below the DOS epoch",
            md.mtime.sec,
        );
        assert!(md.ctime.sec >= dos_epoch, "ctime below the DOS epoch");
        assert!(md.atime.sec >= dos_epoch, "atime below the DOS epoch");

        // The root has no directory entry of its own; it must still stat.
        assert_eq!(root.metadata().unwrap().type_, FileType::Dir);
    }

    /// The DOS calendar only runs 1980..=2107, and `Date::new` panics outside
    /// it. An uninitialised clock (1970) must clamp, not take the kernel down.
    #[test]
    fn dos_time_conversion_is_total() {
        for &secs in &[315_532_800i64, 1_700_000_000, 2_000_000_000] {
            assert_eq!(unix_from_dos(dos_from_unix(secs)), secs, "round trip");
        }
        // Below the floor and above the ceiling clamp instead of panicking.
        assert_eq!(dos_from_unix(0).date.year, 1980);
        assert_eq!(dos_from_unix(-10_000_000_000).date.year, 1980);
        assert_eq!(dos_from_unix(i64::MAX / 2).date.year, 2107);
    }

    /// `read_at`/`write_at` issued a single `fatfs` call, which stops at the
    /// cluster boundary, so every request beyond one cluster came back short.
    #[test]
    fn reads_and_writes_span_clusters() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let file = create_file(&root, "big.bin");

        let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            file.write_at(0, &payload).unwrap(),
            payload.len(),
            "short write across clusters",
        );
        assert_eq!(file.metadata().unwrap().size, payload.len());

        let mut got = vec![0u8; payload.len()];
        assert_eq!(
            file.read_at(0, &mut got).unwrap(),
            payload.len(),
            "short read across clusters",
        );
        assert_eq!(got, payload);

        // A read starting past EOF is empty, not an error and not a wrong-offset
        // read served from the clamped position.
        let mut tail = [0u8; 16];
        assert_eq!(file.read_at(payload.len() + 4096, &mut tail).unwrap(), 0);
    }

    /// A full `readdir` rebuilt the entire name list on every `get_entry`
    /// call — O(n²). The listing cache also has to stay correct across
    /// create/unlink/rename.
    #[test]
    fn readdir_lists_every_entry_once() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let dir = root.create("many", FileType::Dir, 0o755).unwrap();
        for i in 0..200 {
            create_file(&dir, &alloc::format!("f{:03}.txt", i));
        }

        let mut names = Vec::new();
        let mut id = 0;
        while let Ok(name) = dir.get_entry(id) {
            names.push(name);
            id += 1;
        }
        assert_eq!(names[0], ".");
        assert_eq!(names[1], "..");
        let mut rest: Vec<_> = names[2..].to_vec();
        rest.sort();
        rest.dedup();
        assert_eq!(
            rest.len(),
            200,
            "readdir did not list each file exactly once"
        );

        // The cache must notice a removal.
        dir.unlink("f000.txt").unwrap();
        assert_eq!(dir.find("f000.txt").err(), Some(FsError::EntryNotFound));
        let mut count = 0;
        while dir.get_entry(count + 2).is_ok() {
            count += 1;
        }
        assert_eq!(count, 199);

        // …and a rename.
        dir.move_("f001.txt", &dir, "renamed.txt").unwrap();
        assert!(dir.find("renamed.txt").is_ok());
        assert_eq!(dir.find("f001.txt").err(), Some(FsError::EntryNotFound));

        // `get_entry_with_metadata` agrees with `find` + `metadata`.
        let (md, name) = dir.get_entry_with_metadata(2).unwrap();
        let by_lookup = dir.find(&name).unwrap().metadata().unwrap();
        assert_eq!(md.inode, by_lookup.inode);
        assert_eq!(md.size, by_lookup.size);
    }

    /// FAT is case-insensitive; a lookup must find an 8.3 entry stored
    /// upper-case, and the size reported must follow writes.
    #[test]
    fn lookup_is_case_insensitive_and_size_tracks_writes() {
        let (_blk, fs) = mount_default();
        let root = fs.root_inode();
        let file = create_file(&root, "BOOTX64.EFI");
        assert_eq!(file.metadata().unwrap().size, 0);

        file.write_at(0, &[7u8; 1234]).unwrap();
        assert_eq!(
            root.find("bootx64.efi").unwrap().metadata().unwrap().size,
            1234,
            "size did not follow the write through the listing cache",
        );
    }

    /// `sync()`/`fsync()` returned Ok without anything reaching the device.
    #[test]
    fn sync_reaches_the_device() {
        let (blk, fs) = mount_default();
        let root = fs.root_inode();
        let file = create_file(&root, "durable.txt");
        file.write_at(0, b"data").unwrap();

        let before = blk.flushes();
        fs.sync().unwrap();
        assert!(blk.flushes() > before, "fs.sync() never reached the device");

        let before = blk.flushes();
        file.sync_all().unwrap();
        assert!(
            blk.flushes() > before,
            "sync_all() never reached the device"
        );
    }

    /// The raw disk view: a negative seek used to wrap to a huge offset, and a
    /// write past the end of the device returned `Ok(0)` — "wrote nothing",
    /// which spins a caller that retries and loses data either way.
    #[test]
    fn disk_view_rejects_bad_offsets() {
        let nsec = 2048;
        let block = MockBlock::new(nsec);
        let backend = MountBackend::Block(block);
        let dev = device_from_backend(&backend).unwrap();
        let mut disk = FatDisk {
            dev,
            len: nsec as u64 * 512,
            pos: 0,
        };

        assert!(disk.seek(SeekFrom::Current(-1)).is_err());
        assert!(disk.seek(SeekFrom::End(-(nsec as i64) * 512 - 1)).is_err());
        assert_eq!(disk.seek(SeekFrom::End(0)).unwrap(), nsec as u64 * 512);

        // Positioned exactly at the end of the device.
        assert!(disk.write(&[1u8; 16]).is_err());
        assert_eq!(disk.read(&mut [0u8; 16]).unwrap(), 0);
    }

    mod interop {
        //! Cross-validation against the reference FAT tooling. A volume the kernel
        //! wrote has to be a *valid* FAT volume that other systems read the same
        //! way, not merely one our own driver round-trips. Skipped when dosfstools
        //! / mtools are unavailable.
        extern crate std;

        use super::{mount_default, FileType};
        use alloc::string::ToString;
        use alloc::vec;
        use alloc::vec::Vec;
        use rcore_fs::vfs::FileSystem as _;
        use std::process::Command;

        fn have(tool: &str) -> bool {
            Command::new(tool)
                .arg("--help")
                .output()
                .map(|o| o.status.code().is_some())
                .unwrap_or(false)
        }

        fn image_path(name: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(std::format!(
                "eclipse-fat-{}-{}.img",
                std::process::id(),
                name
            ))
        }

        #[test]
        fn a_volume_we_wrote_passes_fsck_vfat_and_mtools() {
            if !have("fsck.vfat") || !have("mdir") {
                std::eprintln!("dosfstools/mtools missing, skipping FAT interop test");
                return;
            }

            let (block, fs) = mount_default();
            let root = fs.root_inode();

            // A little of everything the adapter can do: long names, nested
            // directories, a grown file, a sparse write, a rename and an unlink.
            let dir = root.create("EFI", FileType::Dir, 0o755).unwrap();
            let boot = dir.create("Boot", FileType::Dir, 0o755).unwrap();
            let loader = boot.create("bootx64.efi", FileType::File, 0o644).unwrap();
            let payload: Vec<u8> = (0..70_000).map(|i| (i % 251) as u8).collect();
            loader.write_at(0, &payload).unwrap();

            let grown = root.create("grown.bin", FileType::File, 0o644).unwrap();
            grown.resize(40_000).unwrap();

            let sparse = root.create("sparse.bin", FileType::File, 0o644).unwrap();
            sparse.write_at(0, b"head").unwrap();
            sparse.write_at(20_000, b"tail").unwrap();

            let doomed = root
                .create("a-long-file-name.txt", FileType::File, 0o644)
                .unwrap();
            doomed.write_at(0, b"bye").unwrap();
            root.move_("a-long-file-name.txt", &root, "renamed-long-name.txt")
                .unwrap();
            let tmp = root.create("scratch.tmp", FileType::File, 0o644).unwrap();
            tmp.write_at(0, b"x").unwrap();
            root.unlink("scratch.tmp").unwrap();

            // Sin desmontar nada: `sync()` por sí solo tiene que dejar el
            // volumen en un estado que la herramienta de referencia acepte.
            fs.sync().unwrap();

            let path = image_path("interop");
            std::fs::write(&path, &*block.sectors.lock()).unwrap();

            // `-n` is a read-only check: any inconsistency is a non-zero exit.
            let out = Command::new("fsck.vfat")
                .arg("-n")
                .arg("-v")
                .arg(&path)
                .output()
                .unwrap();
            let report = std::string::String::from_utf8_lossy(&out.stdout).to_string();
            assert!(
                out.status.success(),
                "fsck.vfat rejected the volume:\n{}\n{}",
                report,
                std::string::String::from_utf8_lossy(&out.stderr),
            );
            // `sync()` has to leave the volume looking cleanly unmounted, or
            // Linux and Windows run a repair scan every time they mount it.
            assert!(
                !report.contains("Dirty bit is set"),
                "the volume is still marked dirty after sync:\n{}",
                report,
            );

            // mtools reads the volume the way any foreign FAT implementation would.
            let listing = Command::new("mdir")
                .arg("-i")
                .arg(&path)
                .arg("-/")
                .arg("::/")
                .output()
                .unwrap();
            let listing = std::string::String::from_utf8_lossy(&listing.stdout).to_string();
            for want in [
                "bootx64.efi",
                "grown.bin",
                "sparse.bin",
                "renamed-long-name.txt",
            ] {
                assert!(
                    listing.contains(want),
                    "mdir did not show {}:\n{}",
                    want,
                    listing,
                );
            }
            assert!(
                !listing.contains("scratch.tmp"),
                "an unlinked file is still listed:\n{}",
                listing,
            );

            // …and the bytes have to match, not just the names.
            let got = Command::new("mcopy")
                .arg("-i")
                .arg(&path)
                .arg("::/EFI/Boot/bootx64.efi")
                .arg("-")
                .output()
                .unwrap();
            assert!(got.status.success(), "mcopy failed to read the file back");
            assert_eq!(got.stdout, payload, "mtools read back different bytes");

            let grown_back = Command::new("mcopy")
                .arg("-i")
                .arg(&path)
                .arg("::/grown.bin")
                .arg("-")
                .output()
                .unwrap();
            assert_eq!(
                grown_back.stdout,
                vec![0u8; 40_000],
                "the grown file is not 40000 zero bytes to mtools",
            );

            let sparse_back = Command::new("mcopy")
                .arg("-i")
                .arg(&path)
                .arg("::/sparse.bin")
                .arg("-")
                .output()
                .unwrap();
            let mut want = vec![0u8; 20_004];
            want[..4].copy_from_slice(b"head");
            want[20_000..].copy_from_slice(b"tail");
            assert_eq!(
                sparse_back.stdout, want,
                "the past-the-end write is not where mtools sees it",
            );

            let _ = std::fs::remove_file(&path);
        }
    }
}
