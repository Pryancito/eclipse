//! Qué contesta montar vfat sobre algo que no es FAT.
//!
//! Esto es la segunda mitad del «I/O error» de los pendrives, y la que no
//! tiene nada que ver con el bus USB. `mount` sin `-t` no adivina nada: prueba
//! uno por uno los tipos de `/proc/filesystems` y se queda con el errno del
//! último intento. vfat es el último de la lista, así que su error es el que
//! el usuario lee, y vfat contestaba `DeviceError` --EIO-- a todo, incluido
//! «esto no es FAT».
//!
//! El resultado era que un pendrive de fábrica, sano, con su exFAT puesto por
//! el fabricante, daba `mount: mounting /dev/sdb1 on /mnt/ failed: I/O error`:
//! un mensaje que señala al disco y al driver, y no al formato, que es lo
//! único que pasa. Linux contesta EINVAL --«wrong fs type, bad option, bad
//! superblock»-- y eso es lo que se pide abajo.

use super::block_mount::MountBackend;
use super::fat_mount::FatMountFs;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use lock::Mutex;
use rcore_fs::vfs::FsError;
use zcore_drivers::scheme::{BlockScheme, Scheme};
use zcore_drivers::{DeviceError, DeviceResult};

const SECTOR: usize = 512;
/// 4 MiB: de sobra para que el superbloque de ext (byte 1024) y el de btrfs
/// quepan, y para que nada falle por tamaño.
const SECTORS: usize = 8192;

/// Un disco en memoria al que se le puede pedir que falle: es la diferencia
/// entre «el volumen no es FAT» y «el disco no contesta», que es justo lo que
/// se está separando.
struct MemDisk {
    data: Mutex<Vec<u8>>,
    dead: bool,
}

impl MemDisk {
    fn new(data: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(data),
            dead: false,
        })
    }

    fn dead() -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(vec![0u8; SECTORS * SECTOR]),
            dead: true,
        })
    }
}

impl Scheme for MemDisk {
    fn name(&self) -> &str {
        "memdisk"
    }
}

impl BlockScheme for MemDisk {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        if self.dead {
            return Err(DeviceError::IoError);
        }
        let data = self.data.lock();
        let at = block_id * SECTOR;
        if at + buf.len() > data.len() {
            return Err(DeviceError::InvalidParam);
        }
        buf.copy_from_slice(&data[at..at + buf.len()]);
        Ok(())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        if self.dead {
            return Err(DeviceError::IoError);
        }
        let mut data = self.data.lock();
        let at = block_id * SECTOR;
        if at + buf.len() > data.len() {
            return Err(DeviceError::InvalidParam);
        }
        data[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> DeviceResult {
        Ok(())
    }

    fn block_count(&self) -> usize {
        SECTORS
    }
}

fn mount(disk: Arc<MemDisk>) -> Result<(), FsError> {
    let backend = MountBackend::Block(disk as Arc<dyn BlockScheme>);
    FatMountFs::open(&backend).map(|_| ())
}

/// Un volumen con el nombre de exFAT donde FAT pone el del formateador: es lo
/// que trae de fábrica cualquier pendrive de más de 32 GiB.
fn exfat_volume() -> Vec<u8> {
    let mut d = vec![0u8; SECTORS * SECTOR];
    d[0..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
    d[3..11].copy_from_slice(b"EXFAT   ");
    d[510] = 0x55;
    d[511] = 0xaa;
    d
}

/// Y uno de ext4, que es lo que trae un disco externo que estuvo en un Linux.
fn ext4_volume() -> Vec<u8> {
    let mut d = vec![0u8; SECTORS * SECTOR];
    // `s_magic` en el byte 1080, es decir 1024 + 56.
    d[1080] = 0x53;
    d[1081] = 0xef;
    d
}

/// El caso del encargo, en una línea: un volumen sano que no es FAT no es un
/// error de E/S. Mientras lo fuese, `mount` sin `-t` sólo sabía decir
/// «I/O error», que manda a mirar el cable, el pendrive y el driver USB.
#[test]
fn un_volumen_que_no_es_fat_es_einval_y_no_eio() {
    assert_eq!(
        mount(MemDisk::new(exfat_volume())),
        Err(FsError::WrongFs),
        "un exFAT es «este no es mi sistema de archivos», no un disco roto"
    );
    assert_eq!(
        mount(MemDisk::new(ext4_volume())),
        Err(FsError::WrongFs),
        "un ext4 tampoco es un disco roto"
    );
}

/// Un volumen de ceros no lleva firma de nada y tampoco es FAT: el error es el
/// mismo, porque lo que decide es el superbloque y no si se reconoce el de
/// otro.
#[test]
fn un_volumen_en_blanco_tambien_es_einval() {
    let blank = vec![0u8; SECTORS * SECTOR];
    assert_eq!(mount(MemDisk::new(blank)), Err(FsError::WrongFs));
}

/// Y el error que SÍ es de E/S sigue siéndolo: la distinción no sirve de nada
/// si se come también los fallos de verdad, que son los que hay que mirar en
/// el bus.
#[test]
fn un_disco_que_no_contesta_sigue_siendo_eio() {
    assert_eq!(
        mount(MemDisk::dead()),
        Err(FsError::DeviceError),
        "una lectura que no llega es un error de E/S, no un formato ajeno"
    );
}

/// Lo que `mount` sin `-t` puede llegar a probar. Un tipo que el kernel monta
/// pero no declara aquí es un tipo que la autodetección **no puede encontrar**,
/// y `eclipsefs` --el sistema de archivos propio, el que lleva un disco
/// instalado-- faltaba: sobre él sólo se probaban btrfs y vfat, y el usuario
/// leía el error de vfat.
#[test]
fn proc_filesystems_declara_todo_lo_que_se_puede_montar() {
    let listed = super::procfs::proc_filesystems_for_test();
    for ty in ["eclipsefs", "btrfs", "vfat"] {
        assert!(
            listed.lines().any(|l| l == alloc::format!("\t{}", ty)),
            "{} se puede montar y no sale en /proc/filesystems sin «nodev»: {:?}",
            ty,
            listed
        );
    }
}
