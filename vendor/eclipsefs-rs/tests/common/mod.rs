//! RAM disk and helpers shared by the integration test files.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, Error};

pub const MIB: usize = 1024 * 1024;
pub const BLOCK: usize = 4096;

/// RAM disk that can be resized like a partition growing under the image.
pub struct MemDisk {
    data: Mutex<Vec<u8>>,
}

impl MemDisk {
    pub fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(vec![0xA5; size]),
        })
    }

    pub fn resize(&self, size: usize) {
        self.data.lock().unwrap().resize(size, 0xA5);
    }

    pub fn poke(&self, at: usize, val: u8) {
        self.data.lock().unwrap()[at] ^= val;
    }
}

impl BlockDevice for MemDisk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let d = self.data.lock().unwrap();
        let o = offset as usize;
        if o + buf.len() > d.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&d[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut d = self.data.lock().unwrap();
        let o = offset as usize;
        if o + buf.len() > d.len() {
            return Err(Error::Io);
        }
        d[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
}

pub fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "eclipse".into(),
        fsid: [1; 16],
        chunk_uuid: [2; 16],
        dev_uuid: [3; 16],
        subvol_uuid: [4; 16],
        now: (1_700_000_000, 0),
    }
}

pub fn clock() -> (u64, u32) {
    (1_800_000_000, 5)
}

pub fn fresh(size: usize) -> (Arc<MemDisk>, EclipseFS) {
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    (disk, fs)
}

pub fn remount(disk: &Arc<MemDisk>, fs: EclipseFS) -> EclipseFS {
    let mut fs = fs;
    fs.sync().unwrap();
    drop(fs);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    fs
}

pub fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 7 + seed * 13 + i / 4096) % 251) as u8)
        .collect()
}

pub fn read_all(fs: &mut EclipseFS, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

pub fn write_all(fs: &mut EclipseFS, ino: u64, off: u64, data: &[u8]) {
    let mut done = 0;
    while done < data.len() {
        let n = fs.write(ino, off + done as u64, &data[done..]).unwrap();
        assert!(n > 0);
        done += n;
    }
}

/// Small deterministic generator so failures replay exactly.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
