//! Block device abstraction used by the driver and mkfs (same contract as `btrfs::BlockDevice`).

use crate::Result;
use alloc::sync::Arc;

/// Byte-addressable backing store (whole partition or image file).
pub trait BlockDevice: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()>;
    fn sync(&self) -> Result<()>;
    /// Device size in bytes.
    fn size(&self) -> u64;
}

impl BlockDevice for Arc<dyn BlockDevice> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf)
    }
    fn sync(&self) -> Result<()> {
        (**self).sync()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
}

/// `std::fs::File`-backed device for tests and host-side image generation.
#[cfg(feature = "std")]
pub struct FileDevice {
    file: std::sync::Mutex<std::fs::File>,
    size: u64,
}

#[cfg(feature = "std")]
impl FileDevice {
    pub fn open(file: std::fs::File) -> std::io::Result<Self> {
        let size = file.metadata()?.len();
        Ok(Self {
            file: std::sync::Mutex::new(file),
            size,
        })
    }
}

#[cfg(feature = "std")]
impl BlockDevice for FileDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| crate::Error::Io)?;
        f.read_exact(buf).map_err(|_| crate::Error::Io)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| crate::Error::Io)?;
        f.write_all(buf).map_err(|_| crate::Error::Io)
    }

    fn sync(&self) -> Result<()> {
        self.file
            .lock()
            .unwrap()
            .sync_data()
            .map_err(|_| crate::Error::Io)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::testdisk::Ram;
    use crate::Error;

    /// The kernel hands the driver an `Arc<dyn BlockDevice>` and the driver
    /// treats that as a device in its own right: every call has to reach the
    /// disk underneath unchanged, or the driver writes one disk and reads
    /// another.
    #[test]
    fn a_shared_device_forwards_every_call() {
        let ram = Ram::new(8192);
        let dev: Arc<dyn BlockDevice> = ram.clone();
        assert_eq!(BlockDevice::size(&dev), 8192);
        BlockDevice::write_at(&dev, 4096, b"eclipse").unwrap();
        assert_eq!(ram.peek(4096, 7), b"eclipse");
        let mut back = [0u8; 7];
        BlockDevice::read_at(&dev, 4096, &mut back).unwrap();
        assert_eq!(&back, b"eclipse");
        BlockDevice::sync(&dev).unwrap();
    }

    /// Errors come back through the wrapper too, rather than being turned
    /// into a short read the driver would take for data.
    #[test]
    fn a_shared_device_forwards_errors() {
        let dev: Arc<dyn BlockDevice> = Ram::new(4096);
        let mut buf = [0u8; 16];
        assert_eq!(BlockDevice::read_at(&dev, 4090, &mut buf), Err(Error::Io));
        assert_eq!(BlockDevice::write_at(&dev, 8192, &buf), Err(Error::Io));
    }

    #[cfg(feature = "std")]
    fn image(tag: &str, len: u64) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!(
            "eclipsefs-unit-{}-{}-{:?}.img",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(len).unwrap();
        (path, f)
    }

    /// `FileDevice` is what `xtask` builds root images with: the size is the
    /// file's, and what goes in at an offset comes back from it.
    #[cfg(feature = "std")]
    #[test]
    fn a_file_device_reads_back_what_it_wrote() {
        let (path, f) = image("rw", 1 << 20);
        let dev = FileDevice::open(f).unwrap();
        assert_eq!(dev.size(), 1 << 20);
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        dev.write_at(12_345, &data).unwrap();
        dev.sync().unwrap();
        let mut back = vec![0u8; data.len()];
        dev.read_at(12_345, &mut back).unwrap();
        assert_eq!(back, data);
        // Untouched space is the file's holes: zeros.
        let mut hole = [1u8; 64];
        dev.read_at(0, &mut hole).unwrap();
        assert_eq!(hole, [0u8; 64]);
        std::fs::remove_file(path).unwrap();
    }

    /// A read past the end of the image is an I/O error, never a short read
    /// padded with whatever the buffer held.
    #[cfg(feature = "std")]
    #[test]
    fn a_file_device_fails_reads_past_its_end() {
        let (path, f) = image("eof", 4096);
        let dev = FileDevice::open(f).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(dev.read_at(4090, &mut buf), Err(crate::Error::Io));
        std::fs::remove_file(path).unwrap();
    }

    /// A filesystem made through `FileDevice` mounts through it again, which
    /// is the whole path from `xtask` to the image the kernel boots.
    #[cfg(feature = "std")]
    #[test]
    fn a_file_device_carries_a_filesystem() {
        use crate::testdisk::opts;
        use crate::{EclipseFS, FileKind};
        let (path, f) = image("fs", 16 << 20);
        let dev = Arc::new(FileDevice::open(f).unwrap());
        crate::mkfs::format(&*dev, &opts()).unwrap();
        let mut fs = EclipseFS::mount(dev.clone(), false).unwrap();
        let root = fs.root_ino();
        let ino = fs
            .create(root, "hola", FileKind::Regular, 0o644, 0)
            .unwrap();
        fs.write(ino, 0, b"desde xtask").unwrap();
        fs.sync().unwrap();
        drop(fs);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut fs = EclipseFS::mount(Arc::new(FileDevice::open(f).unwrap()), true).unwrap();
        let ino = fs.lookup(fs.root_ino(), "hola").unwrap();
        let mut buf = [0u8; 11];
        assert_eq!(fs.read(ino, 0, &mut buf).unwrap(), 11);
        assert_eq!(&buf, b"desde xtask");
        std::fs::remove_file(path).unwrap();
    }
}
