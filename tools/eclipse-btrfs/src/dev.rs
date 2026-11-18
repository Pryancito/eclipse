//! The device as btrfs reads and writes it: the FAT server's [`Disk`], whose
//! chunks are in memory the mount holds.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fatserve::disk::{Blocks, Disk};

/// The disk, to be shared with btrfs, which asks for a device it can
/// send between threads.
pub struct Shared<B: Blocks>(pub Disk<B>);

// SAFETY: the disk's memory is a raw mapping, which is why `Disk` is not
// `Send` by itself; it is only ever used under the mutex of [`Dev`], and
// the server has one thread.
unsafe impl<B: Blocks + Send> Send for Shared<B> {}

/// The shared disk.
pub type DiskRef<B> = Arc<Mutex<Shared<B>>>;

/// A [`Disk`] that btrfs can share: it asks for `&self`, and the server
/// flushes the same disk to the device.
pub struct Dev<B: Blocks> {
    disk: DiskRef<B>,
    len: u64,
    /// The bytes btrfs has written, for whoever measures.
    pub written: Arc<AtomicU64>,
    /// In a test: called before each write, to see the memory as a server
    /// that died there leaves it.
    #[cfg(test)]
    pub before_write: Hook,
}

#[cfg(test)]
pub type Hook = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

impl<B: Blocks> Dev<B> {
    pub fn new(disk: DiskRef<B>, len: u64, written: Arc<AtomicU64>) -> Self {
        Dev {
            disk,
            len,
            written,
            #[cfg(test)]
            before_write: Hook::default(),
        }
    }
}

/// The disk under its lock, whatever a panic left it as: the server ends
/// on one (`panic = "abort"`).
pub fn lock<B: Blocks>(disk: &DiskRef<B>) -> std::sync::MutexGuard<'_, Shared<B>> {
    disk.lock().unwrap_or_else(|e| e.into_inner())
}

fn io_error(_: io::Error) -> btrfs::Error {
    btrfs::Error::Io
}

impl<B: Blocks + Send + 'static> btrfs::BlockDevice for Dev<B> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> btrfs::Result<()> {
        if offset
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(btrfs::Error::Io);
        }
        lock(&self.disk)
            .0
            .read_exact_at(offset, buf)
            .map_err(io_error)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> btrfs::Result<()> {
        if offset
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(btrfs::Error::Io);
        }
        self.written.fetch_add(buf.len() as u64, Ordering::Relaxed);
        #[cfg(test)]
        if let Some(hook) = self.before_write.lock().unwrap().as_mut() {
            hook();
        }
        lock(&self.disk)
            .0
            .write_all_at(offset, buf)
            .map_err(io_error)
    }

    /// What btrfs has written is in the disk's memory, which is what a
    /// server after this one starts from: that is all a change asks. The
    /// device has it when the server flushes the disk.
    fn sync(&self) -> btrfs::Result<()> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.len
    }
}
