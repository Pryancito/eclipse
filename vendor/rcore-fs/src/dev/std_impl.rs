#![cfg(any(test, feature = "std"))]

use std::fs::File;
use std::io::{Error, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

impl Device for Mutex<File> {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let offset = offset as u64;
        let mut file = self.lock().unwrap();
        file.seek(SeekFrom::Start(offset))?;
        let len = file.read(buf)?;
        Ok(len)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let offset = offset as u64;
        let mut file = self.lock().unwrap();
        file.seek(SeekFrom::Start(offset))?;
        let len = file.write(buf)?;
        Ok(len)
    }

    fn sync(&self) -> Result<()> {
        let file = self.lock().unwrap();
        file.sync_all()?;
        Ok(())
    }
}

pub struct StdTimeProvider;

impl TimeProvider for StdTimeProvider {
    fn current_time(&self) -> Timespec {
        let duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        Timespec {
            sec: duration.as_secs() as i64,
            nsec: duration.subsec_nanos() as i32,
        }
    }
}

impl From<Error> for DevError {
    fn from(_: Error) -> Self {
        DevError
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file to stand in for a disk, and the directory that has to outlive it.
    fn device(len: u64) -> (tempfile::TempDir, Mutex<File>) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("disk");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("a file to use as a device");
        file.set_len(len).expect("a device with a size");
        (dir, Mutex::new(file))
    }

    #[test]
    fn a_read_and_a_write_each_go_to_the_offset_they_name() {
        let (_dir, dev) = device(4096);
        assert_eq!(dev.write_at(100, b"abcdef").expect("the write"), 6);
        let mut buf = [0u8; 6];
        assert_eq!(dev.read_at(100, &mut buf).expect("the read"), 6);
        assert_eq!(&buf, b"abcdef");
        // And nowhere else. Without the seek, both would work on the file's
        // own cursor, which starts at the front.
        let mut front = [0xffu8; 6];
        assert_eq!(dev.read_at(0, &mut front).expect("a read of the front"), 6);
        assert_eq!(front, [0u8; 6], "the write landed at the front instead");
    }

    /// `read(2)`'s own answer at the end of a file, which is what the callers
    /// in `dev` expect: a count, not an error.
    #[test]
    fn a_read_that_runs_off_the_end_is_short_and_not_an_error() {
        let (_dir, dev) = device(4096);
        let mut buf = [0u8; 16];
        assert_eq!(
            dev.read_at(4090, &mut buf).expect("a read across the end"),
            6
        );
        assert_eq!(dev.read_at(4096, &mut buf).expect("a read at the end"), 0);
        assert_eq!(dev.read_at(8192, &mut buf).expect("a read past the end"), 0);
    }

    #[test]
    fn what_was_written_is_still_there_after_a_sync() {
        let (_dir, dev) = device(512);
        dev.write_at(0, b"eclipse").expect("the write");
        dev.sync().expect("sync of a plain file");
        let mut buf = [0u8; 7];
        assert_eq!(dev.read_at(0, &mut buf).expect("the read"), 7);
        assert_eq!(&buf, b"eclipse");
    }

    /// A device the host will not read from answers `Err`, which is the only
    /// error there is: the `?` in `read_at` has to turn the host's error into
    /// it rather than unwrapping it.
    #[test]
    fn a_read_the_host_refuses_is_an_error_and_not_a_panic() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("write-only");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("a write-only file");
        file.set_len(512).expect("a device with a size");
        let dev = Mutex::new(file);
        let mut buf = [0u8; 8];
        assert_eq!(dev.read_at(0, &mut buf), Err(DevError));
    }

    #[test]
    fn the_clock_hands_out_seconds_and_nanoseconds_in_their_own_fields() {
        let now = StdTimeProvider.current_time();
        assert!(
            now.sec > 1_700_000_000,
            "the seconds hold {}, which is no time since the epoch",
            now.sec
        );
        assert!(
            (0..1_000_000_000).contains(&now.nsec),
            "the nanoseconds hold {}, which is no fraction of a second",
            now.nsec
        );
    }
}
